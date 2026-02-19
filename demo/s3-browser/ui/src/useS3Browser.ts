import { useState, useEffect, useCallback, useRef } from 'react';
import { listObjects, deleteObjects, deletePrefix, uploadObject, getBucket, headObject, hasCredentials } from './s3client';
import useSelection from './useSelection';
import type { S3Object } from './types';

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
  const [headCache, setHeadCache] = useState<Record<string, { storageType?: string; storedSize?: number }>>({});
  const headCacheRef = useRef(headCache);
  headCacheRef.current = headCache;
  const headInflight = useRef(new Set<string>());
  const prefixRef = useRef(prefix);
  prefixRef.current = prefix;
  const isInitialLoad = useRef(true);

  const query = searchQuery.toLowerCase();
  const filteredObjects = query ? objects.filter((o) => o.key.toLowerCase().includes(query)) : objects;
  const filteredFolders = query ? folders.filter((f) => f.toLowerCase().includes(query)) : folders;

  const selection = useSelection(filteredObjects, filteredFolders);

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
        selection.reconcile(objs, dirs);
      })
      .catch(() => {
        setObjects([]);
        setFolders([]);
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

  // Smart auto-refresh: 15s interval, skip when tab is hidden
  useEffect(() => {
    const id = setInterval(() => {
      if (!document.hidden) refresh();
    }, 15000);
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
          .then(({ storageType, storedSize }) => ({ key, storageType, storedSize }))
          .catch(() => ({ key, storageType: undefined, storedSize: undefined }))
      )
    ).then((results) => {
      setHeadCache((prev) => {
        const next = { ...prev };
        for (const r of results) {
          next[r.key] = { storageType: r.storageType, storedSize: r.storedSize };
          headInflight.current.delete(r.key);
        }
        return next;
      });
    });
  }, []);

  const mutate = useCallback(() => {
    refresh();
  }, [refresh]);

  const navigate = useCallback((newPrefix: string) => {
    // Clear data so spinner shows immediately for the new prefix
    setObjects([]);
    setFolders([]);
    setHeadCache({});
    headInflight.current.clear();
    isInitialLoad.current = true;
    setPrefix(newPrefix);
    selection.clearSelection();
    setSearchQuery('');
  }, [selection.clearSelection]);

  const changeBucket = useCallback((newBucket: string) => {
    setBucketState(newBucket);
    setObjects([]);
    setFolders([]);
    setHeadCache({});
    headInflight.current.clear();
    isInitialLoad.current = true;
    setPrefix('');
    selection.clearSelection();
    setRefreshTrigger((k) => k + 1);
  }, [selection.clearSelection]);

  const reconnect = useCallback(() => {
    setObjects([]);
    setFolders([]);
    setHeadCache({});
    headInflight.current.clear();
    isInitialLoad.current = true;
    setPrefix('');
    selection.clearSelection();
    setConnected(hasCredentials());
    setRefreshTrigger((k) => k + 1);
  }, [selection.clearSelection]);

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
    uploadFiles,
    // Status
    deleting,
    // Search
    searchQuery,
    setSearchQuery,
  };
}
