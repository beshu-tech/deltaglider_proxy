import { useState, useCallback } from 'react';
import type { S3Object } from './types';

/** Selection key of a folder row (object rows use the object key). */
const folderKey = (f: string) => `folder:${f}`;

/**
 * Checked rows of the object browser, independent of data fetching. The
 * caller passes the visible rows to `toggleAll` and the fresh listing to
 * `reconcile`, so this hook can run before the listing hook.
 */
export default function useSelection() {
  const [selectedKeys, setSelectedKeys] = useState<Set<string>>(new Set());

  const toggleKey = useCallback((key: string) => {
    setSelectedKeys((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);

  const toggleAll = useCallback((objects: S3Object[], folders: string[]) => {
    setSelectedKeys((prev) => {
      const allKeys = [...folders.map(folderKey), ...objects.map((o) => o.key)];
      return prev.size === allKeys.length ? new Set() : new Set(allKeys);
    });
  }, []);

  /** Prune stale keys after data refresh. */
  const reconcile = useCallback((freshObjects: S3Object[], freshFolders: string[]) => {
    setSelectedKeys((prev) => {
      const validKeys = new Set([...freshObjects.map((o) => o.key), ...freshFolders.map(folderKey)]);
      const next = new Set<string>();
      for (const k of prev) {
        if (validKeys.has(k)) next.add(k);
      }
      return next.size === prev.size ? prev : next;
    });
  }, []);

  const clearSelection = useCallback(() => {
    setSelectedKeys(new Set());
  }, []);

  return { selectedKeys, toggleKey, toggleAll, reconcile, clearSelection };
}
