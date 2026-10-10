import { useState, useEffect, useLayoutEffect, useCallback, useRef, useMemo } from 'react';
import { message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from './queries/keys';
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
import { isSessionExpired, normalizeUiError } from './errorHandling';
import { useOverlayClose } from './hooks/useOverlayClose';
import useSelection from './useSelection';
import { virtualWritableChildren } from './permissions';
import { type ExpandedItem, MAX_BULK_OBJECTS, expandSelection } from './bulkSelection';
import { type BulkAction, type BulkProgress, BulkActionFailed } from './bulkBatches';
import { type BulkDeleteOutcome, deleteInBatches } from './bulkDelete';
import { type BulkTransferOutcome, transferInBatches, transferPlanError } from './bulkTransfer';
import { downloadZip as saveZip, zipPreflightError } from './zipDownload';
// Bulk actions → admin objects API; App gates them on `sessionCaps.canUseBulkActions`
// (the server authorizes each key for a files-only session).
import type { S3Object } from './types';
import { readStorage, writeStorage } from './safeStorage';

const MAX_HEAD_CACHE_SIZE = 5000;
/**
 * Table HEADs in flight at once. Each costs the proxy two backend HEADs; a
 * page of 500 rows used to send them all at once (1,000 backend HEADs behind
 * an HTTP/2 front), and the folder's listing waited behind them.
 */
const HEAD_CONCURRENCY = 6;
// A stable default: `load` depends on writablePrefixes, so a fresh `[]` per
// render re-ran the list effect on every render (an infinite update loop).
const NO_PREFIXES: string[] = [];

interface UseS3BrowserOptions {
  writablePrefixes?: string[];
  /** The savings chip reads an admin-only endpoint: without an admin session
   *  every folder visit logged a 403, so skip the request instead. */
  adminSession?: boolean;
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
  /**
   * False while App shows another view (admin, upload, docs): the hook then
   * neither lists nor auto-refreshes, and the savings chip stays idle. The
   * listing reloads when the browser view comes back. Default true.
   */
  active?: boolean;
}

export default function useS3Browser(options: UseS3BrowserOptions) {
  const {
    writablePrefixes = NO_PREFIXES,
    adminSession = false,
    bucket,
    prefix,
    q,
    object,
    navigateUrl,
    active = true,
  } = options;
  const [refreshTrigger, setRefreshTrigger] = useState(0);
  // The savings chip walks the folder's whole subtree on the proxy: it
  // reloads on a folder change and after a user's change (mutate, the end of
  // a bulk action), never on the auto-refresh tick or after each batch.
  const [savingsTrigger, setSavingsTrigger] = useState(0);
  const [objects, setObjects] = useState<S3Object[]>([]);
  const [folders, setFolders] = useState<string[]>([]);
  const [virtualFolders, setVirtualFolders] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [isTruncated, setIsTruncated] = useState(false);
  // A running bulk delete, copy or move (null = none): its phase and counts for the action bar.
  const [bulkProgress, setBulkProgress] = useState<BulkProgress | null>(null);
  const bulkRunning = bulkProgress !== null;
  const bulkRun = useRef<AbortController | null>(null);
  const [connected, setConnected] = useState(hasCredentials());
  // The search box shows a local draft; the URL's ?q= follows 200ms later
  // (debounced replace, see setSearchQuery). Binding the box to ?q= directly
  // made each keystroke edit the value from BEFORE the previous one, so at
  // normal typing speed characters vanished ("09-03" → "3"). An external ?q=
  // change (Back, a link) resets the draft unless the user is mid-typing.
  const qDebounce = useRef<number | null>(null);
  const [draftQ, setDraftQ] = useState(q);
  useEffect(() => {
    if (qDebounce.current === null) setDraftQ(q);
  }, [q]);
  const searchQuery = draftQ;
  const [showHidden, setShowHiddenState] = useState(() => readStorage('dg-show-hidden') === 'true');
  const setShowHidden = useCallback((v: boolean) => {
    setShowHiddenState(v);
    writeStorage('dg-show-hidden', String(v));
  }, []);
  const [headCache, setHeadCache] = useState<Record<string, { storageType?: string; storedSize?: number; error?: boolean }>>({});
  const [error, setError] = useState<string | null>(null);
  const headCacheRef = useRef(headCache);
  headCacheRef.current = headCache;
  // HEAD enrichment queue: keys waiting for a slot, keys whose HEAD started
  // (until its result is in the cache), finished results not yet rendered,
  // and the abort of the current generation.
  const headQueue = useRef<string[]>([]);
  const headStarted = useRef(new Set<string>());
  const headResults = useRef<{ key: string; storageType?: string; storedSize?: number; error: boolean }[]>([]);
  const headRunning = useRef(0);
  const headAbort = useRef(new AbortController());
  // Bumped by resetBrowseState (bucket/prefix change), which also aborts the
  // HEADs in flight and drops the queued ones. A HEAD of an older generation
  // drops its result instead of writing the previous folder's metadata into
  // the freshly reset cache (same pattern as loadSeq).
  const headGen = useRef(0);
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
  // (the AWS-SDK calls read it). A LAYOUT effect: it runs before every passive
  // effect, the children's included, so neither the list fetch nor a child's
  // first request (the inspector's HEAD on a ?object= deep link) sees the
  // previous bucket.
  useLayoutEffect(() => {
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
    headAbort.current.abort();
    headAbort.current = new AbortController();
    headQueue.current = [];
    headStarted.current.clear();
    headResults.current = [];
    headRunning.current = 0;
    headGen.current += 1;
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

  /** Reload the listing only (the auto-refresh tick, each bulk batch). */
  const refresh = useCallback(() => {
    setRefreshTrigger((k) => k + 1);
  }, []);

  // Sequence token for load(). Each call increments loadSeq; only the *latest*
  // load is allowed to commit results. This replaces the previous single-flight
  // guard, which silently dropped concurrent loads (e.g. fast prefix/bucket
  // changes), letting the older request commit stale data into state.
  const loadSeq = useRef(0);
  const queryClient = useQueryClient();

  const load = useCallback(() => {
    if (!hasCredentials()) {
      setConnected(false);
      setLoading(false);
      return;
    }
    // Another view is shown: list nothing. The URL bucket is '' there, so
    // the fallback below used to list the last bucket's root every 60 s on
    // Settings. The listing reloads when the browser view comes back.
    if (!active) {
      setLoading(false);
      setRefreshing(false);
      return;
    }
    // Don't load if no bucket is selected yet — the Sidebar will set the initial
    // bucket and trigger a refresh. Without this guard, reconnect() fires a load
    // with an empty bucket before the Sidebar mounts, producing a false "No objects"
    // state that flashes before the real content appears.
    // DO NOT REMOVE: this prevents a race between reconnect() and Sidebar mount.
    // The URL bucket is a real input (and a dep): a bucket switch reloads by
    // design. listObjects() reads the s3client's module bucket, so sync it here
    // rather than rely on the setBucket effect having run first.
    const listBucket = bucket || getBucket();
    if (listBucket && listBucket !== getBucket()) setBucket(listBucket);
    if (!listBucket) {
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
        // Every browser mutation (upload, delete, copy/move, new folder) ends
        // in a reload, so this is the one place the TopBar size/count pill
        // learns the bucket changed — it has no other trigger to refetch.
        void queryClient.invalidateQueries({ queryKey: qk.bucketUsage(listBucket) });
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
  }, [active, bucket, prefix, reconcile, writablePrefixes, queryClient]);

  useEffect(load, [load, refreshTrigger]);

  // Smart auto-refresh: 60s interval, only on the browser view, skip when tab is hidden
  useEffect(() => {
    if (!active) return;
    const id = setInterval(() => {
      if (!document.hidden) refresh();
    }, 60000);
    return () => clearInterval(id);
  }, [refresh, active]);

  // HEAD enrichment: at most HEAD_CONCURRENCY HEADs in flight, the rest
  // queued. Results keep the per-key error flag (a hardcoded error:false once
  // masked HEAD failures) and render a few at a time, not one render per
  // HEAD. A bucket/prefix change aborts the generation (resetBrowseState).
  const flushHeadResults = useCallback(() => {
    const results = headResults.current;
    if (results.length === 0) return;
    headResults.current = [];
    setHeadCache((prev) => {
      const next = { ...prev };
      for (const r of results) {
        next[r.key] = r.error ? { error: true } : { storageType: r.storageType, storedSize: r.storedSize, error: false };
        headStarted.current.delete(r.key);
      }
      // Evict oldest entries if cache exceeds max size
      const keys = Object.keys(next);
      if (keys.length > MAX_HEAD_CACHE_SIZE) {
        const toRemove = keys.slice(0, keys.length - MAX_HEAD_CACHE_SIZE);
        for (const k of toRemove) delete next[k];
      }
      return next;
    });
  }, []);

  const pumpHeads = useCallback(() => {
    const gen = headGen.current;
    const { signal } = headAbort.current;
    while (headRunning.current < HEAD_CONCURRENCY && headQueue.current.length > 0) {
      const key = headQueue.current.shift()!;
      headStarted.current.add(key);
      headRunning.current += 1;
      headObject(key, undefined, signal)
        .then(({ storageType, storedSize }) => ({ key, storageType, storedSize, error: false }))
        .catch(() => ({ key, storageType: undefined, storedSize: undefined, error: true }))
        .then((result) => {
          if (gen !== headGen.current) return; // reset since: the counters belong to the new generation
          headRunning.current -= 1;
          headResults.current.push(result);
          const idle = headRunning.current === 0 && headQueue.current.length === 0;
          if (idle || headResults.current.length >= HEAD_CONCURRENCY) flushHeadResults();
          pumpHeads();
        });
    }
  }, [flushHeadResults]);

  /**
   * HEAD the visible page's files that have no metadata yet. The keys of
   * this call replace the keys still queued from an earlier call: the table
   * asks for the page on screen, and an older page's keys are off screen.
   */
  const enrichKeys = useCallback((keys: string[]) => {
    const cache = headCacheRef.current;
    headQueue.current = keys.filter((k) => !(k in cache) && !headStarted.current.has(k));
    pumpHeads();
  }, [pumpHeads]);

  /** After a user's change: reload the listing and the savings chip. */
  const mutate = useCallback(() => {
    refresh();
    setSavingsTrigger((k) => k + 1);
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
  const setSearchQuery = useCallback((next: string) => {
    setDraftQ(next);
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

  /**
   * Expand the selection against `currentBucket` via the ONE shared expander
   * (a few folders listed at once). Throws (before any mutation) when a folder
   * has more keys than the server lists, so no bulk action runs on a partial
   * folder.
   */
  const expandSelected = useCallback(
    (currentBucket: string, options?: Parameters<typeof expandSelection>[2]) =>
      expandSelection(selectedKeys, (pfx) => listAllUnderPrefix(currentBucket, pfx), options),
    [selectedKeys],
  );

  /**
   * One bulk run (delete, copy, move), the v2.0.4 bulk-delete shape: list the
   * selection's folders (a few at once), then send the items in batches, one
   * request at a time. The action bar shows `bulkProgress`; `cancelBulk`
   * stops the run after the batch in flight. Resolves with the outcome (also
   * after a cancel); rejects with a session expiry as-is (the bar sends the
   * user to sign-in), and with any other error plus how many objects went
   * before it. The listing and the savings chip reload once when it ends.
   */
  const runBulk = useCallback(
    async <O extends { cancelled: boolean; failed: number }>(
      action: BulkAction,
      run: {
        /** List the selection; `opts` carries the abort and the listing progress. */
        expand: (opts: NonNullable<Parameters<typeof expandSelection>[2]>) => Promise<ExpandedItem[]>;
        /** Send the items in batches; `sent(done, went)` after each one. */
        send: (items: ExpandedItem[], signal: AbortSignal, sent: (done: number, went: number) => void) => Promise<O>;
        /** The outcome of a run cancelled while it listed. */
        cancelledWhileListing: () => O;
        /** Reload the listing after each batch: the sources leave the page as the run goes on. */
        reloadPerBatch: boolean;
      },
    ): Promise<O> => {
      const ctl = new AbortController();
      bulkRun.current = ctl;
      const stopping = () => ctl.signal.aborted;
      let total = 0;
      let went = 0;
      setBulkProgress({ action, phase: 'listing', listed: 0, folders: 0, keysFound: 0, stopping: false });
      try {
        const items = await run.expand({
          signal: ctl.signal,
          onProgress: (p) => setBulkProgress({ action, phase: 'listing', ...p, stopping: stopping() }),
        });
        total = items.length;
        setBulkProgress({ action, phase: 'sending', done: 0, total, stopping: stopping() });
        const outcome = await run.send(items, ctl.signal, (done, n) => {
          went = n;
          setBulkProgress({ action, phase: 'sending', done, total, stopping: stopping() });
          if (run.reloadPerBatch) refresh();
        });
        // A cancelled or partly failed run keeps the selection for a retry; the
        // reloads already dropped the entries that are gone.
        if (!outcome.cancelled && outcome.failed === 0) clearSelection();
        return outcome;
      } catch (e) {
        if (ctl.signal.aborted && e === ctl.signal.reason) return run.cancelledWhileListing();
        if (isSessionExpired(e)) throw e;
        throw new BulkActionFailed(action, e, went, total);
      } finally {
        bulkRun.current = null;
        setBulkProgress(null);
        // Once for the whole run (a failed batch may have done part of its
        // work): the listing and the savings chip, not after each batch.
        if (total > 0) mutate();
      }
    },
    [clearSelection, refresh, mutate],
  );

  /** Delete the selection (see runBulk). The run has no total limit. */
  const bulkDelete = useCallback((): Promise<BulkDeleteOutcome> => {
    // Snapshot the bucket: a bucket switch mid-run must not move the deletes.
    const bucket = getBucket();
    return runBulk('delete', {
      expand: (opts) => expandSelected(bucket, opts),
      send: (items, signal, sent) =>
        deleteInBatches(
          items.map((i) => i.source),
          (batch) => bulkDeleteObjects({ bucket, keys: batch }),
          { signal, onBatch: (p) => sent(p.done, p.deleted) },
        ),
      cancelledWhileListing: () => ({ total: 0, deleted: 0, failed: 0, failures: [], cancelled: true }),
      reloadPerBatch: true,
    });
  }, [runBulk, expandSelected]);

  /**
   * Copy or move the selection to `destBucket`/`destPrefix` (see runBulk).
   * A selected folder `foo/` keeps its own name under the destination
   * (`foo/bar/a.txt` lands at `destPrefix + foo/bar/a.txt`); a selected object
   * lands under its basename. The listing stops once the selection passes
   * the server's cap (MAX_BULK_OBJECTS), and the whole plan is checked before
   * the first batch: the server sees one batch at a time. For a move, the
   * server removes a batch's sources only when every copy of that batch
   * succeeded.
   */
  const bulkTransfer = useCallback(
    (action: 'copy' | 'move', destBucket: string, destPrefix: string): Promise<BulkTransferOutcome> => {
      // Snapshot the source bucket ONCE so the listing and the requests run
      // against the same bucket even if the user switches buckets mid-run.
      const sourceBucket = getBucket();
      const request = action === 'copy' ? bulkCopyObjects : bulkMoveObjects;
      return runBulk(action, {
        expand: async (opts) => {
          // The folder's own marker key (empty suffix) is not copied.
          const items = (await expandSelected(sourceBucket, { ...opts, maxKeys: MAX_BULK_OBJECTS })).filter(
            (i) => i.relative !== '',
          );
          const problem = transferPlanError(items, sourceBucket, destBucket, destPrefix);
          if (problem) throw new Error(problem);
          return items;
        },
        send: (items, signal, sent) =>
          transferInBatches(
            action,
            items,
            (batch) =>
              request({
                source_bucket: sourceBucket,
                dest_bucket: destBucket,
                dest_prefix: destPrefix,
                items: batch.map(({ source, relative }) => ({ source_key: source, relative })),
              }),
            { signal, onBatch: (p) => sent(p.done, p.went) },
          ),
        cancelledWhileListing: () => ({
          action,
          total: 0,
          succeeded: 0,
          failed: 0,
          deleted: 0,
          failures: [],
          cancelled: true,
        }),
        reloadPerBatch: action === 'move',
      });
    },
    [runBulk, expandSelected],
  );
  const bulkCopy = useCallback(
    (destBucket: string, destPrefix: string) => bulkTransfer('copy', destBucket, destPrefix),
    [bulkTransfer],
  );
  const bulkMove = useCallback(
    (destBucket: string, destPrefix: string) => bulkTransfer('move', destBucket, destPrefix),
    [bulkTransfer],
  );

  /** Stop the running bulk action: the batch in flight finishes, no new one starts. */
  const cancelBulk = useCallback(() => {
    bulkRun.current?.abort();
    setBulkProgress((p) => (p ? { ...p, stopping: true } : p));
  }, []);

  // Leaving the page stops the run after the batch in flight: ask first.
  const runningAction = bulkProgress?.action;
  useEffect(() => {
    if (!runningAction) return;
    const warn = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = `A bulk ${runningAction} is running. Leaving the page stops it.`;
      return e.returnValue;
    };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  }, [runningAction]);

  const downloadZip = useCallback(async () => {
    // The proxy builds the archive; the browser resolves the selection and
    // saves the response. See zipDownload.ts for how failures stay visible.
    const bucket = getBucket();
    // The listing stops once the selection passes what one ZIP takes.
    const keys = (await expandSelected(bucket, { maxKeys: MAX_BULK_OBJECTS })).map((i) => i.source);
    const url = bulkZipDownloadUrl(bucket, keys);
    const blocked = zipPreflightError(keys.length, url.length);
    if (blocked) throw new Error(blocked);
    const filename = `deltaglider-${new Date().toISOString().slice(0, 10)}.zip`;
    const outcome = await saveZip(url, filename);
    if (outcome === 'saved') message.success(`Saved ${filename}`);
    if (outcome === 'started') {
      message.info(
        "The ZIP download started. If it fails, your browser's download list shows it.",
        8,
      );
    }
  }, [expandSelected]);

  // Per-prefix delta savings, fetched from the server-side endpoint that
  // owns the canonical (reference-aware) math. Previously this was a
  // client-side accumulator over `headCache`, which undercounted by one
  // `reference.bin` per deltaspace and could read "100% saved" for ROR-
  // shaped buckets. The server now owns the algorithm; the SPA just
  // renders. See `src/api/admin/savings.rs` for the wire shape.
  const [deltaSummary, setDeltaSummary] = useState<DeltaSummary | null>(null);
  useEffect(() => {
    if (!connected || !active) return;
    // Use the URL-derived `bucket` prop (not the module-level getBucket()) so the
    // fetched savings always match the bucket the URL is showing. Reading global
    // state here raced the setBucket() sync effect: switching bucket A→B without
    // changing prefix would fetch A's savings and commit them under B's view.
    if (!bucket) return;
    if (!adminSession) {
      setDeltaSummary(null);
      return;
    }
    // Leaving the folder aborts the request: a walk the user no longer
    // looks at must not keep running beside the next folder's walk.
    const request = new AbortController();
    setDeltaSummary((prev) => (prev ? { ...prev, loading: true } : null));
    getPrefixSavings(bucket, prefix, request.signal)
      .then((resp) => {
        if (request.signal.aborted) return;
        if (!resp) {
          // Admin endpoint refused (no admin session) — keep the chip
          // hidden rather than guessing client-side.
          setDeltaSummary(null);
          return;
        }
        setDeltaSummary(summaryFromResponse(resp));
      })
      .catch(() => {
        if (request.signal.aborted) return;
        setDeltaSummary(null);
      });
    return () => request.abort();
  }, [connected, active, bucket, prefix, savingsTrigger, adminSession]);

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
    cancelBulk,
    downloadZip,
    // Status
    bulkRunning,
    bulkProgress,
    // Search
    searchQuery,
    setSearchQuery,
    // Hidden files
    showHidden,
    setShowHidden,
  };
}
