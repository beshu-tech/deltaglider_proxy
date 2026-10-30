import { useState, useCallback, useMemo } from 'react';
import type { S3Object } from './types';
import useSelection from './useSelection';
import { useBrowserLocation, type NavigateUrl } from './browser/useBrowserLocation';
import { useObjectListing } from './browser/useObjectListing';
import { useVisibleEntries } from './browser/useVisibleEntries';
import { useBulkActions } from './browser/useBulkActions';
import { useDeltaSummary } from './browser/useDeltaSummary';

// A stable default: `load` depends on writablePrefixes, so a fresh `[]` per
// render re-ran the list effect on every render (an infinite update loop).
const NO_PREFIXES: string[] = [];

interface UseS3BrowserOptions {
  writablePrefixes?: string[];
  /** The savings chip reads an admin-only endpoint: without an admin session
   *  every folder visit logged a 403, so skip the request instead. */
  adminSession?: boolean;
  /** The browser location from the URL (see `useBrowserLocation`). */
  bucket: string;
  prefix: string;
  q: string;
  object: string;
  navigateUrl: NavigateUrl;
  /**
   * False while App shows another view (admin, upload, docs): the hook then
   * neither lists nor auto-refreshes, and the savings chip stays idle. The
   * listing reloads when the browser view comes back. Default true.
   */
  active?: boolean;
}

export type S3Browser = ReturnType<typeof useS3Browser>;

/**
 * The object browser: composes location, selection, listing, visible rows,
 * bulk actions and the savings chip into the one object App renders from.
 */
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

  const location = useBrowserLocation({ bucket, prefix, q, object, navigateUrl });
  const selection = useSelection();
  const { clearSelection, reconcile, selectedKeys } = selection;
  const listing = useObjectListing({ bucket, prefix, active, writablePrefixes, reconcile, clearSelection });
  const visible = useVisibleEntries(listing.objects, listing.folders, listing.virtualFolders, location.searchQuery);

  const { objects: visibleObjects, folders: visibleFolders } = visible;
  const { toggleAll: toggleAllOf } = selection;
  const toggleAll = useCallback(
    () => toggleAllOf(visibleObjects, visibleFolders),
    [toggleAllOf, visibleObjects, visibleFolders],
  );

  // The savings chip walks the folder's whole subtree on the proxy: it
  // reloads on a folder change and after a user's change (mutate, the end of
  // a bulk action), never on the auto-refresh tick or after each batch.
  const [savingsTrigger, setSavingsTrigger] = useState(0);
  const { refresh } = listing;
  /** After a user's change: reload the listing and the savings chip. */
  const mutate = useCallback(() => {
    refresh();
    setSavingsTrigger((k) => k + 1);
  }, [refresh]);

  const bulk = useBulkActions({ selectedKeys, clearSelection, refresh, mutate });
  const deltaSummary = useDeltaSummary({
    bucket,
    prefix,
    active,
    connected: listing.connected,
    adminSession,
    trigger: savingsTrigger,
  });

  // ── Inspector deep-link (?object=<key>) ──────────────────────────────────
  // The open object lives in the URL (?object=). It is derived from the key:
  // prefer the row already in the list; otherwise a light stub (InspectorPanel
  // HEADs on mount to fill in size/metadata).
  const inspectorObject = useMemo<S3Object | null>(() => {
    if (!object) return null;
    const found = listing.objects.find((o) => o.key === object);
    if (found) return found;
    return { key: object, size: 0, lastModified: '' } as S3Object;
  }, [object, listing.objects]);

  return {
    // Data
    objects: visible.objects,
    folders: visible.folders,
    virtualFolders: visible.virtualFolders,
    allFolders: listing.folders,
    prefix,
    loading: listing.loading,
    refreshing: listing.refreshing,
    isTruncated: listing.isTruncated,
    restListedAt: listing.restListedAt,
    headCache: listing.headCache,
    deltaSummary,
    connected: listing.connected,
    error: listing.error,
    // Selection
    selectedKeys,
    toggleKey: selection.toggleKey,
    toggleAll,
    // Inspector (URL-backed: ?object=)
    inspectorObject,
    openInspector: location.openInspector,
    closeInspector: location.closeInspector,
    // Actions
    navigate: location.navigate,
    changeBucket: location.changeBucket,
    reconnect: listing.reconnect,
    mutate,
    removeObject: listing.removeObject,
    ...bulk,
    // Search
    searchQuery: location.searchQuery,
    setSearchQuery: location.setSearchQuery,
    // Hidden files
    showHidden: visible.showHidden,
    setShowHidden: visible.setShowHidden,
  };
}
