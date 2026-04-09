import { useState, useEffect, useCallback, useRef } from 'react';
import { listObjects, listAllKeys, deleteObjects, deletePrefix, uploadObject, getBucket, setBucket, headObject, hasCredentials, copyObject, getObjectBytes } from './s3client';
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
  const [bucket, setBucketState] = useState(getBucket());
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

  /** Clear objects, folders, head cache, and mark as initial load. */
  const resetBrowseState = useCallback(() => {
    setObjects([]);
    setFolders([]);
    setHeadCache({});
    setError(null);
    headInflight.current.clear();
    isInitialLoad.current = true;
    selection.clearSelection();
  }, [selection.clearSelection]);

  const refresh = useCallback(() => {
    setRefreshTrigger((k) => k + 1);
  }, []);

  const loadingRef = useRef(false);

  const load = useCallback(() => {
    if (loadingRef.current) return; // skip if a request is already in-flight
    if (!hasCredentials()) {
      setConnected(false);
      setLoading(false);
      return;
    }
    loadingRef.current = true;

    // On initial load or prefix/bucket change, show full loading spinner.
    // On background refresh, show subtle refreshing indicator.
    if (isInitialLoad.current) {
      setLoading(true);
    } else {
      setRefreshing(true);
    }

    listObjects(prefix)
      .then(({ objects: objs, folders: dirs, isTruncated: trunc }) => {
        setObjects(objs);
        setFolders(dirs);
        setIsTruncated(trunc);
        setConnected(true);
        setError(null);
        selection.reconcile(objs, dirs);
      })
      .catch((err) => {
        // Keep stale data on error instead of clearing
        setError(err instanceof Error ? err.message : 'Failed to load objects');
        setConnected(false);
      })
      .finally(() => {
        loadingRef.current = false;
        isInitialLoad.current = false;
        setLoading(false);
        setRefreshing(false);
      });
  }, [prefix, bucket, selection.reconcile]);

  useEffect(load, [load, refreshTrigger]);

  // Smart auto-refresh: 60s interval, skip when tab is hidden
  useEffect(() => {
    const id = setInterval(() => {
      if (!document.hidden) refresh();
    }, 60000);
    return () => clearInterval(id);
  }, [refresh]);

  const enrichKeys = useCallback((keys: string[]) => {
    const cache = headCacheRef.current;
    const toFetch = keys.filter((k) => !(k in cache) && !headInflight.current.has(k));
    if (toFetch.length === 0) return;
    for (const k of toFetch) headInflight.current.add(k);
    Promise.all(
      toFetch.map((key) =>
        headObject(key)
          .then(({ storageType, storedSize }) => ({ key, storageType, storedSize, error: false }))
          .catch(() => ({ key, storageType: undefined, storedSize: undefined, error: true }))
      )
    ).then((results) => {
      setHeadCache((prev) => {
        const next = { ...prev };
        for (const r of results) {
          next[r.key] = { storageType: r.storageType, storedSize: r.storedSize, error: false };
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
    setBucketState(newBucket);  // Then update React state
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
    if (selection.selectedKeys.size === 0) return;
    setDeleting(true);
    try {
      const objectKeys: string[] = [];
      const folderPrefixes: string[] = [];
      for (const k of selection.selectedKeys) {
        if (k.startsWith('folder:')) {
          folderPrefixes.push(k.slice('folder:'.length));
        } else {
          objectKeys.push(k);
        }
      }
      await Promise.all([
        objectKeys.length > 0 ? deleteObjects(objectKeys) : Promise.resolve(),
        ...folderPrefixes.map((pfx) => deletePrefix(pfx)),
      ]);
      selection.clearSelection();
      refresh();
    } catch (e) {
      console.error('Bulk delete failed:', e);
    } finally {
      setDeleting(false);
    }
  }, [selection.selectedKeys, selection.clearSelection, refresh]);

  /** Get all object keys from the selection, expanding folders recursively. */
  /** Resolve selected keys, expanding folders recursively to get ALL nested objects. */
  const resolveSelectedKeys = useCallback(async (): Promise<string[]> => {
    const keys: string[] = [];
    for (const k of selection.selectedKeys) {
      if (k.startsWith('folder:')) {
        const pfx = k.slice('folder:'.length);
        const nested = await listAllKeys(pfx);
        keys.push(...nested);
      } else {
        keys.push(k);
      }
    }
    return keys;
  }, [selection.selectedKeys]);

  const bulkCopy = useCallback(async (destBucket: string, destPrefix: string) => {
    const keys = await resolveSelectedKeys();
    const sourceBucket = getBucket();
    let succeeded = 0, failed = 0;
    for (const key of keys) {
      const filename = key.split('/').pop() || key;
      const destKey = destPrefix ? `${destPrefix}${filename}` : filename;
      try {
        await copyObject(sourceBucket, key, destBucket, destKey);
        succeeded++;
      } catch { failed++; }
    }
    refresh();
    return { succeeded, failed };
  }, [resolveSelectedKeys, refresh]);

  const bulkMove = useCallback(async (destBucket: string, destPrefix: string) => {
    const keys = await resolveSelectedKeys();
    const sourceBucket = getBucket();
    // Copy all first
    const copied: string[] = [];
    let failed = 0;
    for (const key of keys) {
      const filename = key.split('/').pop() || key;
      const destKey = destPrefix ? `${destPrefix}${filename}` : filename;
      try {
        await copyObject(sourceBucket, key, destBucket, destKey);
        copied.push(key);
      } catch { failed++; }
    }
    // Only delete sources AFTER all copies succeed
    if (copied.length > 0) {
      await deleteObjects(copied).catch(() => {});
    }
    selection.clearSelection();
    refresh();
    return { succeeded: copied.length, failed };
  }, [resolveSelectedKeys, selection.clearSelection, refresh]);

  const downloadZip = useCallback(async () => {
    const { zipSync } = await import('fflate');
    const keys = await resolveSelectedKeys();
    if (keys.length === 0) return;

    const files: Record<string, Uint8Array> = {};
    for (const key of keys) {
      try {
        const { data, name } = await getObjectBytes(key);
        files[name] = data;
      } catch {
        // Skip failed files
      }
    }

    const zipped = zipSync(files);
    const blob = new Blob([new Uint8Array(zipped)], { type: 'application/zip' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `deltaglider-${new Date().toISOString().slice(0, 10)}.zip`;
    a.click();
    URL.revokeObjectURL(url);
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
