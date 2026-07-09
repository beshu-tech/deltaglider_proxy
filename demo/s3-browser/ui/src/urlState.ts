/**
 * Pure URL <-> app-location mapping for the whole SPA.
 *
 * The address bar is the single source of truth for "where am I": the top-level
 * view, and — for the bucket browser — the active bucket, the current folder
 * prefix, the search filter, and the open inspector object. Folder navigation
 * therefore creates real history entries, so the browser Back/Forward buttons
 * work naturally and deep folder views are reload-safe / shareable.
 *
 * This module is React-free and exhaustively unit-tested. `parseBrowserLocation`
 * and `buildBrowserUrl` round-trip: `parse(build(x))` deep-equals `x` (modulo
 * empty-field normalisation). NEVER hand-concatenate browser URLs — always go
 * through `buildBrowserUrl` so encoding stays correct for S3 keys (spaces, `+`,
 * unicode; `/` is the folder delimiter and is the only reserved char).
 */

export type View = 'browser' | 'upload' | 'metrics' | 'docs' | 'admin';

/** URL path prefix the SPA is served under. `_` is not a valid S3 bucket char. */
export const BASE = '/_/';

const SEGMENT_TO_VIEW: Record<string, View> = {
  '': 'browser',
  browse: 'browser',
  upload: 'upload',
  metrics: 'metrics',
  docs: 'docs',
  admin: 'admin',
};

/** Canonical first URL segment for each view (browser uses `browse`). */
const VIEW_TO_SEGMENT: Record<View, string> = {
  browser: 'browse',
  upload: 'upload',
  metrics: 'metrics',
  docs: 'docs',
  admin: 'admin',
};

/** The non-browser part of the location: view + opaque sub-path (admin/docs/etc.). */
export interface ViewLocation {
  view: View;
  /** Everything after the view segment, joined by `/` (e.g. admin section path). */
  subPath: string;
}

/** The full bucket-browser location encoded in the URL. */
export interface BrowserLocation {
  /** Active bucket, or '' when at the bucket list / no bucket selected. */
  bucket: string;
  /** Current folder prefix (trailing-slash, S3 "common prefix" shape), or ''. */
  prefix: string;
  /** Search filter (?q=), or ''. */
  q: string;
  /** Open inspector object key (?object=), or ''. */
  object: string;
}

/** Strip BASE / leading slash and trailing slashes from a raw pathname. */
function stripBase(pathname: string): string {
  let path = pathname;
  if (path.startsWith(BASE)) path = path.slice(BASE.length);
  else if (path.startsWith('/')) path = path.slice(1);
  return path.replace(/\/+$/, '');
}

/** Parse a pathname into view + opaque sub-path (admin/docs/metrics use subPath). */
export function parseViewLocation(pathname: string): ViewLocation {
  const path = stripBase(pathname);
  const segments = path.split('/');
  const view = SEGMENT_TO_VIEW[segments[0] || ''] ?? 'browser';
  const subPath = segments.slice(1).join('/');
  return { view, subPath };
}

/**
 * Parse the bucket-browser location from a pathname + query string.
 * Only meaningful when the view is `browser`; returns empty fields otherwise.
 * `search` may include or omit the leading `?`.
 */
export function parseBrowserLocation(pathname: string, search: string): BrowserLocation {
  const path = stripBase(pathname);
  const segments = path.split('/').filter((s) => s.length > 0);
  // segments[0] is the view ('browse' or ''); the rest are bucket + prefix.
  const isBrowse = segments.length === 0 || segments[0] === 'browse' || segments[0] === '';
  let bucket = '';
  let prefix = '';
  if (isBrowse && segments.length > 1) {
    bucket = decodeURIComponent(segments[1]);
    if (segments.length > 2) {
      // Re-join the remaining segments as the folder prefix, with a trailing
      // slash (S3 common-prefix shape). Each segment is individually decoded.
      const parts = segments.slice(2).map((s) => decodeURIComponent(s));
      prefix = `${parts.join('/')}/`;
    }
  }

  const params = new URLSearchParams(search.startsWith('?') ? search.slice(1) : search);
  const q = params.get('q') ?? '';
  const object = params.get('object') ?? '';
  return { bucket, prefix, q, object };
}

/**
 * Build the full browser URL (path + query) from a location. Inverse of
 * `parseBrowserLocation`. Always produces a BASE-prefixed, properly-encoded URL.
 */
export function buildBrowserUrl(loc: Partial<BrowserLocation>): string {
  const bucket = loc.bucket ?? '';
  const prefix = loc.prefix ?? '';
  const q = loc.q ?? '';
  const object = loc.object ?? '';

  const segments: string[] = ['browse'];
  if (bucket) {
    segments.push(encodeURIComponent(bucket));
    // Split the prefix into path components (drop the trailing slash + empties)
    // and encode each. `/` stays the delimiter; everything else is encoded.
    const prefixParts = prefix.split('/').filter((p) => p.length > 0);
    for (const p of prefixParts) segments.push(encodeURIComponent(p));
  }
  let url = BASE + segments.join('/');
  // Keep a trailing slash on a bucket/folder so it reads as a directory and
  // round-trips (parse treats trailing slash as the folder marker).
  if (bucket) url += '/';

  const params = new URLSearchParams();
  if (q) params.set('q', q);
  if (object) params.set('object', object);
  const qs = params.toString();
  if (qs) url += `?${qs}`;
  return url;
}

/**
 * Build a non-browser view URL (admin/docs/metrics/upload) from a sub-path
 * and optional query params. The inverse of `parseViewLocation` + the query
 * string: `buildViewUrl('admin', 'jobs', { job: 'replication:foo', tab: 'runs' })`
 * → `/_/admin/jobs?job=replication%3Afoo&tab=runs`.
 *
 * Empty/undefined values are omitted from the query string automatically.
 */
export function buildViewUrl(view: View, subPath = '', query?: Record<string, string>): string {
  const seg = VIEW_TO_SEGMENT[view];
  const clean = subPath.replace(/^\/+/, '').replace(/\/+$/, '');
  let url = BASE + (clean ? `${seg}/${clean}` : seg);
  if (query) {
    const params = new URLSearchParams();
    for (const [k, v] of Object.entries(query)) {
      if (v) params.set(k, v);
    }
    const qs = params.toString();
    if (qs) url += `?${qs}`;
  }
  return url;
}

/**
 * Parse query params from a search string for non-browser views (admin/docs/etc.).
 * Returns a flat key→value map. `search` may include or omit the leading `?`.
 * Complements `parseBrowserLocation` which handles the browser-specific `?q=`
 * and `?object=` params.
 */
export function parseAdminQuery(search: string): Record<string, string> {
  const params = new URLSearchParams(search.startsWith('?') ? search.slice(1) : search);
  const result: Record<string, string> = {};
  for (const [k, v] of params.entries()) {
    result[k] = v;
  }
  return result;
}
