import { useState, useCallback } from 'react';
import type { S3Object } from './types';

/** Manages row selection state independently from data fetching. */
export default function useSelection(objects: S3Object[], folders: string[]) {
  const [selected, setSelected] = useState<S3Object | null>(null);
  const [selectedKeys, setSelectedKeys] = useState<Set<string>>(new Set());

  const toggleKey = useCallback((key: string) => {
    setSelectedKeys((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);

  const toggleAll = useCallback(() => {
    setSelectedKeys((prev) => {
      const allKeys = [
        ...folders.map((f) => `folder:${f}`),
        ...objects.map((o) => o.key),
      ];
      return prev.size === allKeys.length ? new Set() : new Set(allKeys);
    });
  }, [objects, folders]);

  /** Prune stale keys and selected object after data refresh. */
  const reconcile = useCallback((freshObjects: S3Object[], freshFolders: string[]) => {
    setSelected((prev) => {
      if (!prev) return null;
      return freshObjects.find((o) => o.key === prev.key) || null;
    });
    setSelectedKeys((prev) => {
      const validKeys = new Set([
        ...freshObjects.map((o) => o.key),
        ...freshFolders.map((f) => `folder:${f}`),
      ]);
      const next = new Set<string>();
      for (const k of prev) {
        if (validKeys.has(k)) next.add(k);
      }
      return next.size === prev.size ? prev : next;
    });
  }, []);

  const clearSelection = useCallback(() => {
    setSelected(null);
    setSelectedKeys(new Set());
  }, []);

  return {
    selected,
    setSelected,
    selectedKeys,
    toggleKey,
    toggleAll,
    reconcile,
    clearSelection,
  };
}
