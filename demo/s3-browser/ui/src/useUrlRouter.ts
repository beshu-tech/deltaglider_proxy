import { useState, useEffect, useCallback } from 'react';
import {
  BASE,
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

interface UrlRouter extends UrlLocation {
  /**
   * Low-level navigation. `url` is a full BASE-prefixed path (+ optional query),
   * as produced by `buildBrowserUrl` / `buildViewUrl`. Pushes a history entry by
   * default; pass `{ replace: true }` to swap the current entry instead (used for
   * the debounced `?q=` filter so typing doesn't spam history). Stable identity
   * (`useCallback([])`) so it can sit in consumer dependency arrays without
   * cascading re-renders.
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

  // Redirect old hash-based URLs on first load (carried over verbatim).
  useEffect(() => {
    if (window.location.hash.startsWith('#/')) {
      const oldPath = window.location.hash.slice(1); // e.g., "/admin/users"
      window.history.replaceState(null, '', BASE + oldPath.replace(/^\//, ''));
      setLocation(readLocation());
    }
  }, []);

  const navigate = useCallback((url: string, opts?: { replace?: boolean }) => {
    // `url` is already BASE-prefixed by build*Url; tolerate a missing BASE.
    const fullPath = url.startsWith(BASE)
      ? url
      : BASE + url.replace(/^\//, '');
    if (window.location.pathname + window.location.search + window.location.hash === fullPath) {
      return;
    }
    if (opts?.replace) {
      window.history.replaceState(null, '', fullPath);
    } else {
      window.history.pushState(null, '', fullPath);
    }
    setLocation(readLocation());
  }, []);

  useEffect(() => {
    const onPopState = () => {
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
