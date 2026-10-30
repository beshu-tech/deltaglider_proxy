import { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from '../queries/keys';
import { listObjects, getBucket, setBucket, hasCredentials } from '../s3client';
import type { BucketUsage } from '../adminApi';
import { normalizeUiError } from '../errorHandling';
import { virtualWritableChildren } from '../permissions';
import type { S3Object } from '../types';
import { mergeFirstPage } from '../listingRefresh';

interface ListingOptions {
  bucket: string;
  prefix: string;
  /** False while another view is shown: list nothing, no auto-refresh. */
  active: boolean;
  writablePrefixes: string[];
  /** Prune the selection to the fresh rows (called with every listing). */
  reconcile: (objects: S3Object[], folders: string[]) => void;
  /** Drop the selection (folder or bucket change, reconnect). */
  clearSelection: () => void;
}

/**
 * The raw listing of the current folder: load, 60 s auto-refresh (page 1
 * only), stale-response guard and the per-row storage facts.
 */
export function useObjectListing({
  bucket,
  prefix,
  active,
  writablePrefixes,
  reconcile,
  clearSelection,
}: ListingOptions) {
  const [refreshTrigger, setRefreshTrigger] = useState(0);
  const [objects, setObjects] = useState<S3Object[]>([]);
  const [folders, setFolders] = useState<string[]>([]);
  const [virtualFolders, setVirtualFolders] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [isTruncated, setIsTruncated] = useState(false);
  // When the shown rows after the first page were listed (a full listing
  // that went past page 1), or null: the status bar says how old they are.
  const [restListedAt, setRestListedAt] = useState<number | null>(null);
  const [connected, setConnected] = useState(hasCredentials());
  const [error, setError] = useState<string | null>(null);
  const isInitialLoad = useRef(true);

  /** Clear objects, folders, head cache, and mark as initial load. */
  const resetBrowseState = useCallback(() => {
    setObjects([]);
    setFolders([]);
    setVirtualFolders([]);
    setError(null);
    setRestListedAt(null);
    isInitialLoad.current = true;
    clearSelection();
  }, [clearSelection]);

  // The URL drives location: whenever bucket or prefix changes (folder nav,
  // bucket switch, Back/Forward, reload), reset transient browse state so stale
  // rows from the previous folder don't flash before the new listing arrives.
  useEffect(() => {
    resetBrowseState();
  }, [bucket, prefix, resetBrowseState]);

  /** Reload the listing only (each bulk batch; the tick lists page 1 only). */
  const refresh = useCallback(() => {
    setRefreshTrigger((k) => k + 1);
  }, []);
  // The next load lists the first page only and keeps the later pages it
  // already has (the 60 s auto-refresh). Every other load lists them all.
  const firstPageOnly = useRef(false);
  const objectsRef = useRef(objects);
  objectsRef.current = objects;
  const foldersRef = useRef(folders);
  foldersRef.current = folders;

  // Sequence token for load(). Each call increments loadSeq; only the *latest*
  // load is allowed to commit results. This replaces the previous single-flight
  // guard, which silently dropped concurrent loads (e.g. fast prefix/bucket
  // changes), letting the older request commit stale data into state.
  const loadSeq = useRef(0);
  // The seq of the full listing in flight (0 = none). A first-page tick never
  // replaces it: the tick's rows after page 1 come from the listing before the
  // change, so a file uploaded onto page 2 would vanish until the next reload.
  const fullLoadSeq = useRef(0);
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
    // A first listing of the folder lists everything, also on a tick.
    const pageOne = firstPageOnly.current && !isInitialLoad.current;
    firstPageOnly.current = false;
    // A full listing in flight is newer than the tick: skip the tick.
    if (pageOne && fullLoadSeq.current !== 0) return;
    const seq = ++loadSeq.current;
    fullLoadSeq.current = pageOne ? 0 : seq;

    // On initial load or prefix/bucket change, show full loading spinner.
    // On background refresh, show subtle refreshing indicator.
    if (isInitialLoad.current) {
      setLoading(true);
    } else {
      setRefreshing(true);
    }

    listObjects(prefix, pageOne ? 1 : undefined)
      .then((listing) => {
        if (seq !== loadSeq.current) return; // stale response — newer load in flight
        const { objects: objs, folders: dirs } = pageOne
          ? mergeFirstPage({ objects: objectsRef.current, folders: foldersRef.current }, listing)
          : listing;
        if (!pageOne) {
          setIsTruncated(listing.isTruncated);
          setRestListedAt((listing.pages ?? 1) > 1 ? Date.now() : null);
        } else if (listing.continuesAfter === undefined) {
          // The folder fits one page now: the page replaced everything.
          setIsTruncated(false);
          setRestListedAt(null);
        }
        const virtualFolders = virtualWritableChildren(prefix, dirs, writablePrefixes);
        const mergedFolders = virtualFolders.length > 0 ? Array.from(new Set([...dirs, ...virtualFolders])) : dirs;
        setObjects(objs);
        setFolders(mergedFolders);
        setVirtualFolders(virtualFolders);
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
        fullLoadSeq.current = 0;
        isInitialLoad.current = false;
        setLoading(false);
        setRefreshing(false);
      });
  }, [active, bucket, prefix, reconcile, writablePrefixes, queryClient]);

  useEffect(load, [load, refreshTrigger]);

  // Smart auto-refresh: 60s interval, only on the browser view, skip when tab
  // is hidden. A tick missed while hidden runs as soon as the tab shows again.
  useEffect(() => {
    if (!active) return;
    let missed = false;
    const tick = () => {
      firstPageOnly.current = true;
      refresh();
    };
    const id = setInterval(() => {
      if (document.hidden) {
        missed = true;
        return;
      }
      tick();
    }, 60000);
    const onVisible = () => {
      if (document.hidden || !missed) return;
      missed = false;
      tick();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      clearInterval(id);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [refresh, active]);

  // The storage facts of each row, from the listing (`metadata=true`): no
  // HEAD per row. A row the listing has no facts for gets an empty entry, so
  // the table shows the neutral tag instead of waiting for facts.
  const headCache = useMemo(() => {
    const cache: Record<string, { storageType?: string; storedSize?: number; isBaseline?: boolean }> = {};
    for (const o of objects) {
      // `isBaseline`: the listing marks the folder's baseline, so the table
      // chip says "Baseline" as the inspector does (UX harvest C4).
      cache[o.key] = { storageType: o.storageType, storedSize: o.storedSize, isBaseline: o.isBaseline };
    }
    return cache;
  }, [objects]);

  /**
   * After a single delete (the inspector): drop the row and take it off the
   * bucket's size/count pill, with no new listing. The 60 s refresh
   * reconciles both with the backend.
   */
  const removeObject = useCallback((key: string) => {
    const gone = objectsRef.current.find((o) => o.key === key);
    if (!gone) return;
    const rest = objectsRef.current.filter((o) => o.key !== key);
    setObjects(rest);
    reconcile(rest, folders);
    const listBucket = bucket || getBucket();
    queryClient.setQueryData<BucketUsage | null>(qk.bucketUsage(listBucket), (usage) =>
      usage
        ? {
            ...usage,
            object_count: Math.max(0, usage.object_count - 1),
            logical_bytes: Math.max(0, usage.logical_bytes - gone.size),
            stored_bytes: Math.max(0, usage.stored_bytes - (gone.storedSize ?? gone.size)),
          }
        : usage,
    );
  }, [bucket, folders, queryClient, reconcile]);

  const reconnect = useCallback(() => {
    resetBrowseState();
    setConnected(hasCredentials());
    setRefreshTrigger((k) => k + 1);
  }, [resetBrowseState]);

  return {
    objects,
    folders,
    virtualFolders,
    loading,
    refreshing,
    isTruncated,
    restListedAt,
    headCache,
    connected,
    error,
    refresh,
    reconnect,
    removeObject,
  };
}
