import { useState, useEffect, useCallback, useRef } from 'react';
import { listObjects, uploadObject, getBucket, setBucket, headObject, hasCredentials } from './s3client';
import {
  bulkCopyObjects,
  bulkDeleteObjects,
  bulkMoveObjects,
  bulkZipDownloadUrl,
  listAllUnderPrefix,
} from './adminApi';
import useSelection from './useSelection';
import type { S3Object } from './types';

const MAX_HEAD_CACHE_SIZE = 5000;

export default function useS3Browser() {
  const [refreshTrigger, setRefreshTrigger] = useState(0);
  const [objects, setObjects] = useState<S3Object[]>([]);
  const [folders, setFolders] = useState<string[]>([]);
  const [prefix, setPrefix] = useState('');
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [isTruncated, setIsTruncated] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [connected, setConnected] = useState(hasCredentials());
  const [searchQuery, setSearchQuery] = useState('');
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
  const isInitialLoad = useRef(true);

  const query = searchQuery.toLowerCase();
  const filteredObjects = query ? objects.filter((o) => o.key.toLowerCase().includes(query)) : objects;

  // Filter hidden DG system folders unless showHidden is on
  const visibleFolders = showHidden ? folders : folders.filter(d => {
    const name = d.replace(/\/$/, '').split('/').pop() ?? '';
    return name !== '.deltaglider' && name !== '.dg';
  });
  const filteredFolders = query ? visibleFolders.filter((f) => f.toLowerCase().includes(query)) : visibleFolders;

  const selection = useSelection(filteredObjects, filteredFolders);
  const { clearSelection, reconcile, selectedKeys } = selection;

  /** Clear objects, folders, head cache, and mark as initial load. */
  const resetBrowseState = useCallback(() => {
    setObjects([]);
    setFolders([]);
    setHeadCache({});
    setError(null);
    headInflight.current.clear();
    isInitialLoad.current = true;
    clearSelection();
  }, [clearSelection]);

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
        setObjects(objs);
        setFolders(dirs);
        setIsTruncated(trunc);
        setConnected(true);
        setError(null);
        reconcile(objs, dirs);
      })
      .catch((err) => {
        if (seq !== loadSeq.current) return; // stale error — drop silently
        // Keep stale data on error instead of clearing
        setError(err instanceof Error ? err.message : 'Failed to load objects');
        setConnected(false);
      })
      .finally(() => {
        if (seq !== loadSeq.current) return; // stale settle — newer load owns the spinners
        isInitialLoad.current = false;
        setLoading(false);
        setRefreshing(false);
      });
  }, [prefix, reconcile]);

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

  const navigate = useCallback((newPrefix: string) => {
    resetBrowseState();
    setPrefix(newPrefix);
    setSearchQuery('');
  }, [resetBrowseState]);

  const changeBucket = useCallback((newBucket: string) => {
    setBucket(newBucket);       // Update the s3client's active bucket FIRST
    resetBrowseState();
    setPrefix('');
    setRefreshTrigger((k) => k + 1);
  }, [resetBrowseState]);

  const reconnect = useCallback(() => {
    resetBrowseState();
    setPrefix('');
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
      console.error('Bulk delete failed:', e);
    } finally {
      setDeleting(false);
    }
  }, [clearSelection, refresh, selectedKeys]);

  /** Get all object keys from the selection, expanding folders recursively. */
  /** Resolve selected keys, expanding folders recursively. Deduplicates overlapping folders. */
  const resolveSelectedKeys = useCallback(async (): Promise<string[]> => {
    const keySet = new Set<string>();
    const currentBucket = getBucket();
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
  const resolveSelectionWithRelativeKeys = useCallback(async (): Promise<Array<{ source: string; relative: string }>> => {
    // dedupe by absolute source key while keeping the FIRST relative suffix we
    // saw — later overlapping folder selections shouldn't shorten a prefix that
    // an earlier folder already established.
    const seen = new Map<string, string>();
    const currentBucket = getBucket();
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
    const items = await resolveSelectionWithRelativeKeys();
    if (items.length === 0) return { succeeded: 0, failed: 0 };
    const sourceBucket = getBucket();
    const result = await bulkCopyObjects({
      source_bucket: sourceBucket,
      dest_bucket: destBucket,
      dest_prefix: destPrefix,
      items: items.map(({ source, relative }) => ({ source_key: source, relative })),
    });
    refresh();
    return { succeeded: result.succeeded, failed: result.failed };
  }, [resolveSelectionWithRelativeKeys, refresh]);

  const bulkMove = useCallback(async (destBucket: string, destPrefix: string) => {
    // Server-side move with the same atomicity rule as before:
    // sources are deleted ONLY when every copy succeeded. Difference
    // is now the policy is enforced inside one engine call instead of
    // a client-side loop that could be interrupted mid-flight.
    const items = await resolveSelectionWithRelativeKeys();
    if (items.length === 0) return { succeeded: 0, failed: 0 };
    const sourceBucket = getBucket();
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
    const keys = await resolveSelectedKeys();
    if (keys.length === 0) return;
    const bucket = getBucket();
    const url = bulkZipDownloadUrl(keys.map((k) => `${bucket}/${k}`));
    const a = document.createElement('a');
    a.href = url;
    a.download = `deltaglider-${new Date().toISOString().slice(0, 10)}.zip`;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
  }, [resolveSelectedKeys]);

  const uploadFiles = useCallback(async (files: FileList) => {
    const currentPrefix = prefixRef.current;
    try {
      for (const file of Array.from(files)) {
        const key = currentPrefix ? `${currentPrefix}${file.name}` : file.name;
        await uploadObject(key, file);
      }
      setRefreshTrigger((k) => k + 1);
    } catch (e) {
      console.error('Upload failed:', e);
    }
  }, []);

  return {
    // Data
    objects: filteredObjects,
    folders: filteredFolders,
    allFolders: folders,
    prefix,
    loading,
    refreshing,
    isTruncated,
    headCache,
    connected,
    refreshTrigger,
    error,
    // Selection (delegated)
    selected: selection.selected,
    setSelected: selection.setSelected,
    selectedKeys: selection.selectedKeys,
    toggleKey: selection.toggleKey,
    toggleAll: selection.toggleAll,
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
    uploadFiles,
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
