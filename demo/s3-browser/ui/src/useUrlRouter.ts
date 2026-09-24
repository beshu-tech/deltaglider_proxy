import { useState, useEffect, useCallback, useRef } from 'react';
import { getDirtySections } from './useDirtySection';
import {
  BASE,
  isAdminPageLeave,
  parseViewLocation,
  parseBrowserLocation,
  type View,
  type BrowserLocation,
} from './urlState';

/**
 * The full app location derived from `window.location`. `view` / `subPath`
 * drive the top-level view switch (admin/docs/metrics/upload) exactly as the
 * old inline `usePathRouter` did; `browser` carries the bucket-browser state
 * (bucket / prefix / q / object) parsed from the path + query string;
 * `search` exposes the raw query string for non-browser views (admin deep-
 * links like `?job=…&tab=…`).
 */
interface UrlLocation {
  view: View;
  subPath: string;
  browser: BrowserLocation;
  /** Raw query string (with leading `?`) for non-browser views. */
  search: string;
}

function readLocation(): UrlLocation {
  const { view, subPath } = parseViewLocation(window.location.pathname);
  const browser = parseBrowserLocation(window.location.pathname, window.location.search);
  return { view, subPath, browser, search: window.location.search };
}

function currentUrl(): string {
  return window.location.pathname + window.location.search + window.location.hash;
}

/**
 * THE unsaved-edits gate for every SPA navigation. Leaving an admin page
 * unmounts its panels, and an unmounting panel drops its dirty mark together
 * with the edits; `beforeunload` does not fire for pushState. So when the move
 * leaves the admin page (`isAdminPageLeave`) and any section is dirty, ask.
 * Returns true when the navigation may proceed.
 *
 * Covered, because every one of them goes through `navigate` or popstate:
 * sidebar + mobile drawer, the ⌘K palette (nav + "Back to Browser" + "Setup
 * wizard"), the header Back, Settings/Docs shortcuts and account-menu links,
 * in-panel links (user → group, admission → buckets), the session-expiry
 * redirect, and browser Back/Forward. NOT covered: full page loads (reload,
 * OAuth redirects, `<a href>`), which `beforeunload` already guards.
 */
function confirmLeave(fromUrl: string, toUrl: string): boolean {
  if (!isAdminPageLeave(fromUrl, toUrl)) return true;
  const dirty = [...getDirtySections()];
  if (dirty.length === 0) return true;
  return window.confirm(
    `You have unsaved changes (${dirty.join(', ')}). Leave this page and discard them?`
  );
}

interface UrlRouter extends UrlLocation {
  /**
   * Low-level navigation. `url` is a full BASE-prefixed path (+ optional query),
   * as produced by `buildBrowserUrl` / `buildViewUrl`. Pushes a history entry by
   * default; pass `{ replace: true }` to swap the current entry instead (used for
   * the debounced `?q=` filter so typing doesn't spam history). Stable identity
   * (`useCallback([])`) so it can sit in consumer dependency arrays without
   * cascading re-renders. A move that would discard unsaved admin edits asks
   * first (`confirmLeave`) and does nothing when the operator cancels.
   */
  navigate: (url: string, opts?: { replace?: boolean }) => void;
}

/**
 * Owns the pushState/popstate lifecycle for the whole SPA and exposes the
 * current location (re-derived from `window.location` on every navigation and
 * on Back/Forward). Built on the pure helpers in `urlState.ts`.
 *
 * Carries over verbatim from the old inline `usePathRouter`:
 *  - the legacy `#/...` hash → path redirect on mount.
 *
 * NOTE: the old `skipNext` guard (armed on pushState so our own navigation
 * wouldn't double-handle via popstate) has been removed — pushState and
 * replaceState never emit popstate, so the flag sat armed and swallowed the
 * first real Back press.  Removing it fixes the "first Back does nothing"
 * bug without any downside.
 */
export function useUrlRouter(): UrlRouter {
  const [location, setLocation] = useState<UrlLocation>(readLocation);
  // The URL the app last rendered. Browser Back/Forward has already changed
  // window.location by the time popstate fires; this is where to return to
  // when the operator cancels the leave.
  const committedUrl = useRef(currentUrl());

  // Redirect old hash-based URLs on first load (carried over verbatim).
  useEffect(() => {
    if (window.location.hash.startsWith('#/')) {
      const oldPath = window.location.hash.slice(1); // e.g., "/admin/users"
      window.history.replaceState(null, '', BASE + oldPath.replace(/^\//, ''));
      committedUrl.current = currentUrl();
      setLocation(readLocation());
    }
  }, []);

  const navigate = useCallback((url: string, opts?: { replace?: boolean }) => {
    // `url` is already BASE-prefixed by build*Url; tolerate a missing BASE.
    const fullPath = url.startsWith(BASE)
      ? url
      : BASE + url.replace(/^\//, '');
    if (currentUrl() === fullPath) {
      return;
    }
    if (!confirmLeave(currentUrl(), fullPath)) return;
    if (opts?.replace) {
      window.history.replaceState(null, '', fullPath);
    } else {
      window.history.pushState(null, '', fullPath);
    }
    committedUrl.current = currentUrl();
    setLocation(readLocation());
  }, []);

  useEffect(() => {
    const onPopState = () => {
      if (!confirmLeave(committedUrl.current, currentUrl())) {
        // Cancelled: the browser already moved. Put the page's URL back (a
        // new entry; the direction of the Back/Forward press is unknown) and
        // keep rendering the page, so the panel and its edits stay mounted.
        window.history.pushState(null, '', committedUrl.current);
        return;
      }
      committedUrl.current = currentUrl();
      setLocation(readLocation());
    };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  return {
    view: location.view,
    subPath: location.subPath,
    browser: location.browser,
    search: location.search,
    navigate,
  };
}
