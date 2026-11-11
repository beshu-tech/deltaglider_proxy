import { useState, useCallback } from 'react';
import type { S3Object } from './types';
import { type RowKey, folderRowKey, objectRowKey } from './rowKey';

/** The row keys of the given folders and objects (see `rowKey.ts`). */
function rowKeysOf(objects: S3Object[], folders: string[]): Set<RowKey> {
  return new Set<RowKey>([...folders.map(folderRowKey), ...objects.map((o) => objectRowKey(o.key))]);
}

/**
 * Checked rows of the object browser, independent of data fetching. The
 * caller passes the visible rows to `toggleAll` and to `reconcile`, so this
 * hook can run before the listing hook. A checked row is always a row that
 * is shown: `reconcile` drops the rest (a refresh, a filter, a folder change).
 */
export default function useSelection() {
  const [selectedKeys, setSelectedKeys] = useState<Set<RowKey>>(new Set());

  const toggleKey = useCallback((key: RowKey) => {
    setSelectedKeys((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);

  /** Check every shown row, or, when every shown row is checked, none. */
  const toggleAll = useCallback((objects: S3Object[], folders: string[]) => {
    setSelectedKeys((prev) => {
      const shown = rowKeysOf(objects, folders);
      const allChecked = shown.size > 0 && [...shown].every((k) => prev.has(k));
      return allChecked ? new Set() : shown;
    });
  }, []);

  /** Keep only the checked rows that are in the given rows. */
  const reconcile = useCallback((shownObjects: S3Object[], shownFolders: string[]) => {
    setSelectedKeys((prev) => {
      const shown = rowKeysOf(shownObjects, shownFolders);
      const next = new Set<RowKey>();
      for (const k of prev) {
        if (shown.has(k)) next.add(k);
      }
      return next.size === prev.size ? prev : next;
    });
  }, []);

  const clearSelection = useCallback(() => {
    setSelectedKeys(new Set());
  }, []);

  return { selectedKeys, toggleKey, toggleAll, reconcile, clearSelection };
}
