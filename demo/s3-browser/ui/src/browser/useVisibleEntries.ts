import { useState, useCallback, useMemo } from 'react';
import { readStorage, writeStorage } from '../safeStorage';
import type { S3Object } from '../types';

/**
 * The rows the table shows: the search filter applied, and the DG system
 * folders hidden unless "Show system files" is on (persisted).
 */
export function useVisibleEntries(
  objects: S3Object[],
  folders: string[],
  virtualFolders: string[],
  searchQuery: string,
) {
  const [showHidden, setShowHiddenState] = useState(() => readStorage('dg-show-hidden') === 'true');
  const setShowHidden = useCallback((v: boolean) => {
    setShowHiddenState(v);
    writeStorage('dg-show-hidden', String(v));
  }, []);

  const query = searchQuery.toLowerCase();
  const visibleObjects = query ? objects.filter((o) => o.key.toLowerCase().includes(query)) : objects;

  // Memoized: ObjectTable's folder-size effect depends on this array, and a
  // fresh array per render made that effect send GET /usage in a loop.
  const visibleFolders = useMemo(() => {
    const shown = showHidden ? folders : folders.filter((d) => {
      const name = d.replace(/\/$/, '').split('/').pop() ?? '';
      return name !== '.deltaglider' && name !== '.dg';
    });
    return query ? shown.filter((f) => f.toLowerCase().includes(query)) : shown;
  }, [folders, showHidden, query]);
  const visibleVirtualFolders = visibleFolders.filter((folder) => virtualFolders.includes(folder));

  return {
    objects: visibleObjects,
    folders: visibleFolders,
    virtualFolders: visibleVirtualFolders,
    showHidden,
    setShowHidden,
  };
}
