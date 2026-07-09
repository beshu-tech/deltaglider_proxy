import { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { message } from 'antd';
import { listObjects, getBucket, setBucket, headObject, hasCredentials } from './s3client';
import { buildBrowserUrl } from './urlState';
import {
  bulkCopyObjects,
  bulkDeleteObjects,
  bulkMoveObjects,
  bulkZipDownloadUrl,
  getPrefixSavings,
  listAllUnderPrefix,
} from './adminApi';
import { type DeltaSummary, summaryFromResponse } from './deltaSummary';
import { normalizeUiError } from './errorHandling';
import { useOverlayClose } from './hooks/useOverlayClose';
import useSelection from './useSelection';
import { virtualWritableChildren } from './permissions';
// Bulk actions → admin objects API; gate with `sessionCapabilities.canBulkOps` / `adminGui`.
import type { S3Object } from './types';

const MAX_HEAD_CACHE_SIZE = 5000;

interface UseS3BrowserOptions {
  writablePrefixes?: string[];
  /**
   * The browser location, derived from the URL by `useUrlRouter().browser`.
   * The URL is the single source of truth for where-am-I: bucket / current
   * folder prefix / search filter / open inspector object. Folder navigation
   * therefore creates real history entries (Back/Forward work) and reload
   * restores the folder.
   */
  bucket: string;
  prefix: string;
  q: string;
  object: string;
  /** Low-level URL navigation from the router (push by default, {replace} to swap). */
  navigateUrl: (url: string, opts?: { replace?: boolean }) => void;
}

export default function useS3Browser(options: UseS3BrowserOptions) {
  const {
    writablePrefixes = [],
    bucket,
    prefix,
    q,
    object,
    navigateUrl,
  } = options;
  const [refreshTrigger, setRefreshTrigger] = useState(0);
  const [objects, setObjects] = useState<S3Object[]>([]);
  const [folders, setFolders] = useState<string[]>([]);
  const [virtualFolders, setVirtualFolders] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [isTruncated, setIsTruncated] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [connected, setConnected] = useState(hasCredentials());
  const searchQuery = q;
  const [showHidden, setShowHiddenState] = useState(() => localStorage.getItem('dg-show-hidden') === 'true');
  const setShowHidden = useCallback((v: boolean) => {
    setShowHiddenState(v);
    localStorage.setItem('dg-show-hidden', String(v));
  }, []);
  const [headCache, setHeadCache] = useState<Record<string, { storageType?: string; storedSize?: number; error?: boolean }>>({});
  const [error, setError] = useState<string | null>(null);
  const headCacheRef = useRef(headCache);
  headCacheRef.current = headCache;
  const headInflight = useRef(new Set<string>());
  const prefixRef = useRef(prefix);
  prefixRef.current = prefix;
  // bucket/object tracked in refs too so the debounced search write reads the
  // LATEST values when its timer fires, not the values captured when the
  // keystroke was typed (see setSearchQuery).
  const bucketRef = useRef(bucket);
  bucketRef.current = bucket;
  const objectRef = useRef(object);
  objectRef.current = object;
  const isInitialLoad = useRef(true);

  // Keep the s3client's module-level active bucket in sync with the URL bucket
  // (the AWS-SDK calls read it). Runs before the list-fetch effect on a bucket
  // change so listObjects() targets the right bucket.
  useEffect(() => {
    if (bucket && bucket !== getBucket()) {
      setBucket(bucket);
    }
  }, [bucket]);

  const query = searchQuery.toLowerCase();
  const filteredObjects = query ? objects.filter((o) => o.key.toLowerCase().includes(query)) : objects;

  // Filter hidden DG system folders unless showHidden is on
  const visibleFolders = showHidden ? folders : folders.filter(d => {
    const name = d.replace(/\/$/, '').split('/').pop() ?? '';
    return name !== '.deltaglider' && name !== '.dg';
  });
  const filteredFolders = query ? visibleFolders.filter((f) => f.toLowerCase().includes(query)) : visibleFolders;
  const filteredVirtualFolders = filteredFolders.filter((folder) => virtualFolders.includes(folder));

  const selection = useSelection(filteredObjects, filteredFolders);
  const { clearSelection, reconcile, selectedKeys } = selection;

  /** Clear objects, folders, head cache, and mark as initial load. */
  const resetBrowseState = useCallback(() => {
    setObjects([]);
    setFolders([]);
    setVirtualFolders([]);
    setHeadCache({});
    setError(null);
    headInflight.current.clear();
    isInitialLoad.current = true;
    clearSelection();
  }, [clearSelection]);

  // The URL now drives location: whenever bucket or prefix changes (folder nav,
  // bucket switch, Back/Forward, reload), reset transient browse state so stale
  // rows from the previous folder don't flash before the new listing arrives.
  // Previously this was done imperatively inside navigate()/changeBucket().
  useEffect(() => {
    resetBrowseState();
  }, [bucket, prefix, resetBrowseState]);

  const refresh = useCallback(() => {
    setRefreshTrigger((k) => k + 1);
  }, []);

  // Sequence token for load(). Each call increments loadSeq; only the *latest*
  // load is allowed to commit results. This replaces the previous single-flight
  // guard, which silently dropped concurrent loads (e.g. fast prefix/bucket
  // changes), letting the older request commit stale data into state.
  const loadSeq = useRef(0);

  const load = useCallback(() => {
    if (!hasCredentials()) {
      setConnected(false);
      setLoading(false);
      return;
    }
    // Don't load if no bucket is selected yet — the Sidebar will set the initial
    // bucket and trigger a refresh. Without this guard, reconnect() fires a load
    // with an empty bucket before the Sidebar mounts, producing a false "No objects"
    // state that flashes before the real content appears.
    // DO NOT REMOVE: this prevents a race between reconnect() and Sidebar mount.
    if (!getBucket()) {
      setLoading(false);
      setRefreshing(false);
      setConnected(true);
      return;
    }
    const seq = ++loadSeq.current;

    // On initial load or prefix/bucket change, show full loading spinner.
    // On background refresh, show subtle refreshing indicator.
    if (isInitialLoad.current) {
      setLoading(true);
    } else {
      setRefreshing(true);
    }

    listObjects(prefix)
      .then(({ objects: objs, folders: dirs, isTruncated: trunc }) => {
        if (seq !== loadSeq.current) return; // stale response — newer load in flight
        const virtualFolders = virtualWritableChildren(prefix, dirs, writablePrefixes);
        const mergedFolders = virtualFolders.length > 0 ? Array.from(new Set([...dirs, ...virtualFolders])) : dirs;
        setObjects(objs);
        setFolders(mergedFolders);
        setVirtualFolders(virtualFolders);
        setIsTruncated(trunc);
        setConnected(true);
        setError(null);
        reconcile(objs, mergedFolders);
      })
      .catch((err) => {
        if (seq !== loadSeq.current) return; // stale error — drop silently
        // Keep stale data on error instead of clearing
        setError(normalizeUiError(err, 'Failed to load objects'));
        setConnected(false);
        setVirtualFolders([]);
      })
      .finally(() => {
        if (seq !== loadSeq.current) return; // stale settle — newer load owns the spinners
        isInitialLoad.current = false;
        setLoading(false);
        setRefreshing(false);
      });
  }, [prefix, reconcile, writablePrefixes]);

  useEffect(load, [load, refreshTrigger]);

  // Smart auto-refresh: 60s interval, skip when tab is hidden
  useEffect(() => {
    const id = setInterval(() => {
      if (!document.hidden) refresh();
    }, 60000);
    return () => clearInterval(id);
  }, [refresh]);

  // Track the latest enrich generation. If keys is replaced (prefix/bucket change)
  // before in-flight HEADs settle, we still want to safely write per-key results
  // — but cache writes themselves are idempotent and keyed, so we only need to
  // preserve the per-result error flag. The previous bug hardcoded error:false
  // on every result, masking HEAD failures so ObjectTable never showed warnings.
  const enrichKeys = useCallback((keys: string[]) => {
    const cache = headCacheRef.current;
    const toFetch = keys.filter((k) => !(k in cache) && !headInflight.current.has(k));
    if (toFetch.length === 0) return;
    for (const k of toFetch) headInflight.current.add(k);
    Promise.all(
      toFetch.map((key) =>
        headObject(key)
          .then(({ storageType, storedSize }) => ({ key, storageType, storedSize, error: false as const }))
          .catch(() => ({ key, storageType: undefined, storedSize: undefined, error: true as const }))
      )
    ).then((results) => {
      setHeadCache((prev) => {
        const next = { ...prev };
        for (const r of results) {
          next[r.key] = r.error
            ? { error: true }
            : { storageType: r.storageType, storedSize: r.storedSize, error: false };
          headInflight.current.delete(r.key);
        }
        // Evict oldest entries if cache exceeds max size
        const keys = Object.keys(next);
        if (keys.length > MAX_HEAD_CACHE_SIZE) {
          const toRemove = keys.slice(0, keys.length - MAX_HEAD_CACHE_SIZE);
          for (const k of toRemove) delete next[k];
        }
        return next;
      });
    });
  }, []);

  const mutate = useCallback(() => {
    refresh();
  }, [refresh]);

  // ── Inspector deep-link (?object=<key>) ──────────────────────────────────
  // The "which object is open in the inspector drawer" state lives in the URL
  // (?object=). Opening a row PUSHES that entry, so Back/Esc closes the drawer
  // and the URL deep-links to a file (reload re-opens it). The inspector object
  // is derived from the key: prefer the row already in the list; otherwise a
  // light stub (InspectorPanel HEADs on mount to fill in size/metadata).
  const inspectorObject = useMemo<S3Object | null>(() => {
    if (!object) return null;
    const found = objects.find((o) => o.key === object);
    if (found) return found;
    return { key: object, size: 0, lastModified: '' } as S3Object;
  }, [object, objects]);

  // Direct-load-safe close for the inspector overlay (Tier 1.6 fix).
  const { markPushed: markInspectorPushed, closeOverlay: closeInspectorOverlay } = useOverlayClose();

  const openInspector = useCallback((key: string) => {
    // Fall back to the s3client's active bucket if the URL-derived bucket is
    // still empty (e.g. landed at /_/browse before the URL gained the bucket
    // segment). buildBrowserUrl drops everything when bucket is empty, which
    // would otherwise no-op the navigation.
    navigateUrl(buildBrowserUrl({ bucket: bucket || getBucket(), prefix, q, object: key }));
    markInspectorPushed();
  }, [navigateUrl, bucket, prefix, q, markInspectorPushed]);

  const closeInspector = useCallback(() => {
    if (!object) return;
    // Direct-load-safe: if we pushed the ?object= entry, Back pops it cleanly.
    // If the page was loaded with ?object= already present (shared link),
    // replace the URL to drop it instead of walking out of the SPA.
    closeInspectorOverlay(
      buildBrowserUrl({ bucket: bucket || getBucket(), prefix, q }),
      navigateUrl,
    );
  }, [object, closeInspectorOverlay, navigateUrl, bucket, prefix, q]);

  // Folder navigation: PUSH a new history entry for the new prefix (same bucket,
  // drop any active search). Back returns to the parent folder. The URL change
  // re-derives prefix and triggers the reset + list effects above.
  const navigate = useCallback((newPrefix: string) => {
    // Fall back to the s3client's active bucket when the URL-derived bucket is
    // empty. At bare /_/browse, `bucket` is '' until the URL gains the segment;
    // buildBrowserUrl({ bucket: '', ... }) drops the prefix and yields /_/browse,
    // so a folder click would silently no-op (the v1.3.1 "folders don't open" bug).
    navigateUrl(buildBrowserUrl({ bucket: bucket || getBucket(), prefix: newPrefix }));
  }, [navigateUrl, bucket]);

  // Bucket switch: PUSH; prefix + search reset to root of the new bucket.
  const changeBucket = useCallback((newBucket: string) => {
    setBucket(newBucket); // keep s3client in sync immediately (the effect also does this)
    navigateUrl(buildBrowserUrl({ bucket: newBucket }));
  }, [navigateUrl]);

  // Search filter lives in the URL as ?q=, written as a debounced REPLACE so
  // typing doesn't spam the history stack (each keystroke swaps the current
  // entry rather than pushing). REPLACE also means Back from a filtered view
  // leaves the folder rather than undoing keystrokes.
  const qDebounce = useRef<number | null>(null);
  const setSearchQuery = useCallback((next: string) => {
    if (qDebounce.current !== null) window.clearTimeout(qDebounce.current);
    qDebounce.current = window.setTimeout(() => {
      qDebounce.current = null;
      // Read bucket/prefix/object from refs (latest values), not from the
      // closure captured when the keystroke was typed. Otherwise a bucket/
      // prefix change during the 200ms window would write a stale URL — and if
      // the captured bucket was '' (at bare /_/browse), buildBrowserUrl would
      // drop the prefix entirely. Fall back to getBucket() for the same reason
      // navigate() does.
      navigateUrl(
        buildBrowserUrl({
          bucket: bucketRef.current || getBucket(),
          prefix: prefixRef.current,
          q: next,
          object: objectRef.current,
        }),
        { replace: true },
      );
    }, 200);
  }, [navigateUrl]);
  useEffect(() => () => {
    if (qDebounce.current !== null) window.clearTimeout(qDebounce.current);
  }, []);

  const reconnect = useCallback(() => {
    resetBrowseState();
    setConnected(hasCredentials());
    setRefreshTrigger((k) => k + 1);
  }, [resetBrowseState]);

  const bulkDelete = useCallback(async () => {
    if (selectedKeys.size === 0) return;
    setDeleting(true);
    try {
      // Server-side bulk delete: collect every absolute key, expand
      // folder selections via the same admin /list endpoint we use
      // for copy/move, then a single batched POST. Replaces what used
      // to be `s3client.deleteObjects` (SDK 1000-key batches) plus a
      // per-folder `deletePrefix` call.
      const bucket = getBucket();
      const allKeys = new Set<string>();
      for (const k of selectedKeys) {
        if (k.startsWith('folder:')) {
          const pfx = k.slice('folder:'.length);
          if (!pfx) continue;
          const { keys } = await listAllUnderPrefix(bucket, pfx);
          for (const nk of keys) allKeys.add(nk);
        } else {
          allKeys.add(k);
        }
      }
      if (allKeys.size > 0) {
        await bulkDeleteObjects({ bucket, keys: Array.from(allKeys) });
      }
      clearSelection();
      refresh();
    } catch (e) {
      const msg = normalizeUiError(e, 'Bulk delete failed');
      setError(msg);
      message.error(msg);
    } finally {
      setDeleting(false);
    }
  }, [clearSelection, refresh, selectedKeys]);

  /** Get all object keys from the selection, expanding folders recursively. */
  /** Resolve selected keys, expanding folders recursively. Deduplicates overlapping folders. */
  const resolveSelectedKeys = useCallback(async (currentBucket: string): Promise<string[]> => {
    const keySet = new Set<string>();
    for (const k of selectedKeys) {
      if (k.startsWith('folder:')) {
        const pfx = k.slice('folder:'.length);
        if (!pfx) continue; // Reject empty prefix to avoid listing entire bucket
        // Server-side recursion (replaces the previous AWS-SDK
        // listObjectsV2 loop the browser used to run).
        const { keys: nested } = await listAllUnderPrefix(currentBucket, pfx);
        for (const nk of nested) keySet.add(nk);
      } else {
        keySet.add(k);
      }
    }
    return Array.from(keySet);
  }, [selectedKeys]);

  /**
   * Resolve the selection into [absolute-source-key, relative-dest-suffix] pairs.
   *
   * Semantics:
   * - When the user selects a folder `foo/`, that prefix is the "common prefix"
   *   for everything underneath it: `foo/a.txt` becomes relative-suffix `a.txt`
   *   and `foo/bar/a.txt` becomes `bar/a.txt`. Destination keys are then
   *   `destPrefix + relative-suffix`, preserving the folder structure.
   * - When the user selects a single object directly, the relative-suffix is
   *   just its basename (matches the previous flat behavior for direct picks).
   * - If two different sources resolve to the same destination key, throw
   *   loudly BEFORE any copy starts. The previous code would silently overwrite
   *   when two siblings shared a basename across nested folders.
   */
  const resolveSelectionWithRelativeKeys = useCallback(async (currentBucket: string): Promise<Array<{ source: string; relative: string }>> => {
    // dedupe by absolute source key while keeping the FIRST relative suffix we
    // saw — later overlapping folder selections shouldn't shorten a prefix that
    // an earlier folder already established.
    const seen = new Map<string, string>();
    for (const k of selectedKeys) {
      if (k.startsWith('folder:')) {
        const pfx = k.slice('folder:'.length);
        if (!pfx) continue; // Reject empty prefix to avoid listing entire bucket
        const { keys: nested } = await listAllUnderPrefix(currentBucket, pfx);
        for (const nk of nested) {
          if (seen.has(nk)) continue;
          // Strip the selected folder prefix; if for some reason the listing
          // doesn't start with `pfx` (shouldn't happen, but be defensive),
          // fall back to the basename so we at least don't blow up.
          const relative = nk.startsWith(pfx) ? nk.slice(pfx.length) : (nk.split('/').pop() || nk);
          if (relative) seen.set(nk, relative);
        }
      } else {
        if (seen.has(k)) continue;
        const filename = k.split('/').pop() || k;
        seen.set(k, filename);
      }
    }
    return Array.from(seen, ([source, relative]) => ({ source, relative }));
  }, [selectedKeys]);

  const bulkCopy = useCallback(async (destBucket: string, destPrefix: string) => {
    // Phase B: server-side orchestration. The proxy's engine handles
    // the per-key retrieve+store loop, collision detection, and atomic
    // bookkeeping. Pre-migration the browser ran this loop itself via
    // @aws-sdk/client-s3, with no atomicity and ~250 KB of SDK code
    // shipped down the wire.
    // Snapshot the source bucket ONCE so the folder-expansion listing and the
    // copy execute against the same bucket even if the user switches buckets
    // mid-operation (selection clears on switch, but an in-flight op kept its
    // own closure + re-read getBucket(), which could straddle two buckets).
    const sourceBucket = getBucket();
    const items = await resolveSelectionWithRelativeKeys(sourceBucket);
    if (items.length === 0) return { succeeded: 0, failed: 0 };
    const result = await bulkCopyObjects({
      source_bucket: sourceBucket,
      dest_bucket: destBucket,
      dest_prefix: destPrefix,
      items: items.map(({ source, relative }) => ({ source_key: source, relative })),
    });
    // Clear selection only on the post-await success path (matches bulkMove /
    // bulkDelete): a thrown/rejected bulkCopyObjects above preserves the
    // selection so the user can retry.
    clearSelection();
    refresh();
    return { succeeded: result.succeeded, failed: result.failed };
  }, [clearSelection, resolveSelectionWithRelativeKeys, refresh]);

  const bulkMove = useCallback(async (destBucket: string, destPrefix: string) => {
    // Server-side move with the same atomicity rule as before:
    // sources are deleted ONLY when every copy succeeded. Difference
    // is now the policy is enforced inside one engine call instead of
    // a client-side loop that could be interrupted mid-flight.
    // Snapshot the source bucket once (see bulkCopy) so listing + move can't
    // straddle a mid-operation bucket switch.
    const sourceBucket = getBucket();
    const items = await resolveSelectionWithRelativeKeys(sourceBucket);
    if (items.length === 0) return { succeeded: 0, failed: 0 };
    const result = await bulkMoveObjects({
      source_bucket: sourceBucket,
      dest_bucket: destBucket,
      dest_prefix: destPrefix,
      items: items.map(({ source, relative }) => ({ source_key: source, relative })),
    });
    clearSelection();
    refresh();
    return { succeeded: result.succeeded, failed: result.failed };
  }, [clearSelection, refresh, resolveSelectionWithRelativeKeys]);

  const downloadZip = useCallback(async () => {
    // Phase B: archive assembly moves to the proxy. The browser just
    // resolves the selection and triggers a same-origin download. No
    // SDK GETs in JS, no in-memory zip buffer, no 500 MB cap on JS
    // heap.
    const bucket = getBucket();
    const keys = await resolveSelectedKeys(bucket);
    if (keys.length === 0) return;
    const url = bulkZipDownloadUrl(keys.map((k) => `${bucket}/${k}`));
    const a = document.createElement('a');
    a.href = url;
    a.download = `deltaglider-${new Date().toISOString().slice(0, 10)}.zip`;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
  }, [resolveSelectedKeys]);

  // Per-prefix delta savings, fetched from the server-side endpoint that
  // owns the canonical (reference-aware) math. Previously this was a
  // client-side accumulator over `headCache`, which undercounted by one
  // `reference.bin` per deltaspace and could read "100% saved" for ROR-
  // shaped buckets. The server now owns the algorithm; the SPA just
  // renders. See `src/api/admin/savings.rs` for the wire shape.
  const [deltaSummary, setDeltaSummary] = useState<DeltaSummary | null>(null);
  useEffect(() => {
    if (!connected) return;
    // Use the URL-derived `bucket` prop (not the module-level getBucket()) so the
    // fetched savings always match the bucket the URL is showing. Reading global
    // state here raced the setBucket() sync effect: switching bucket A→B without
    // changing prefix would fetch A's savings and commit them under B's view.
    if (!bucket) return;
    let cancelled = false;
    setDeltaSummary((prev) => (prev ? { ...prev, loading: true } : null));
    getPrefixSavings(bucket, prefix)
      .then((resp) => {
        if (cancelled) return;
        if (!resp) {
          // Admin endpoint refused (no admin session) — keep the chip
          // hidden rather than guessing client-side.
          setDeltaSummary(null);
          return;
        }
        setDeltaSummary(summaryFromResponse(resp));
      })
      .catch(() => {
        if (cancelled) return;
        setDeltaSummary(null);
      });
    return () => {
      cancelled = true;
    };
  }, [connected, bucket, prefix, refreshTrigger]);

  return {
    // Data
    objects: filteredObjects,
    folders: filteredFolders,
    virtualFolders: filteredVirtualFolders,
    allFolders: folders,
    prefix,
    loading,
    refreshing,
    isTruncated,
    headCache,
    deltaSummary,
    connected,
    refreshTrigger,
    error,
    // Selection (delegated)
    selected: selection.selected,
    setSelected: selection.setSelected,
    selectedKeys: selection.selectedKeys,
    toggleKey: selection.toggleKey,
    toggleAll: selection.toggleAll,
    // Inspector (URL-backed: ?object=)
    inspectorObject,
    openInspector,
    closeInspector,
    // Actions
    navigate,
    changeBucket,
    reconnect,
    mutate,
    enrichKeys,
    bulkDelete,
    bulkCopy,
    bulkMove,
    downloadZip,
    // Status
    deleting,
    // Search
    searchQuery,
    setSearchQuery,
    // Hidden files
    showHidden,
    setShowHidden,
  };
}
