/** Binary-unit byte label ("1.5 KB"). Sign-aware (a negative delta reads
 *  "-1.5 KB"); sub-byte input stays in B; the unit clamps at PB; non-finite
 *  input renders "—". */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes)) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  const sign = bytes < 0 ? '-' : '';
  const abs = Math.abs(bytes);
  const i = abs < 1 ? 0 : Math.min(units.length - 1, Math.floor(Math.log(abs) / Math.log(1024)));
  if (i === 0) return `${sign}${Math.round(abs)} B`;
  return `${sign}${(abs / Math.pow(1024, i)).toFixed(1)} ${units[i]}`;
}

/** Extract the display name from a full S3 key given the current prefix */
export function displayName(key: string, prefix: string): string {
  return key.startsWith(prefix) ? key.slice(prefix.length) : key;
}

/** Last path segment of an S3 key (the bare filename). Falls back to the key itself. */
export function getFileName(key: string): string {
  return key.split('/').pop() || key;
}

/** Trigger a browser download of `blob` as `filename` via a transient object URL. */
export function downloadBlobAsFile(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** "1 item" / "3 items" — count-aware pluralisation. */
export function pluralize(count: number, singular: string, plural = singular + 's'): string {
  return `${count} ${count === 1 ? singular : plural}`;
}

/** The noun alone, for callers that format the number themselves:
 *  `${n.toLocaleString()} ${noun(n, 'object')}`. */
export function noun(count: number, singular: string, plural = singular + 's'): string {
  return count === 1 ? singular : plural;
}

/** Split a prefix path into breadcrumb segments */
export function prefixSegments(prefix: string): { label: string; prefix: string }[] {
  if (!prefix) return [];
  const parts = prefix.replace(/\/$/, '').split('/');
  return parts.map((part, i) => ({
    label: part,
    prefix: parts.slice(0, i + 1).join('/') + '/',
  }));
}

/**
 * Parent prefix one level up from `prefix`, or `''` (bucket root) when already
 * at the top. `"a/b/c/"` → `"a/b/"`, `"a/"` → `""`, `""` → `""`. Used by the
 * keyboard "up a folder" navigation (← / Backspace).
 */
export function parentPrefix(prefix: string): string {
  if (!prefix) return '';
  const trimmed = prefix.replace(/\/$/, '');
  const idx = trimmed.lastIndexOf('/');
  return idx === -1 ? '' : trimmed.slice(0, idx + 1);
}

/**
 * Compact duration — THE unit vocabulary for every age / relative-time label:
 * "47s", "5m", "3h 21m", "3d", "2mo", "1y". Hours carry their minutes; every
 * other unit stands alone. Negative input clamps to "0s".
 */
export function formatDuration(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d}d`;
  if (d < 365) return `${Math.floor(d / 30)}mo`;
  return `${Math.floor(d / 365)}y`;
}

/**
 * Relative time in the `formatDuration` vocabulary: "just now", "5s ago",
 * "2h 21m ago". `when` is a Date, an ISO string, or epoch MILLISECONDS; null or
 * unparseable → "—". Pass `now` when a ticking caller re-renders live.
 *
 * A future instant is clock skew by default and reads "just now". Pass
 * `future: true` for genuinely scheduled times (e.g. a next retry): it then
 * reads "in 5m".
 */
export function relativeTime(
  when: Date | string | number | null | undefined,
  opts: { now?: Date | number; future?: boolean } = {},
): string {
  if (when == null) return '—';
  const t = new Date(when).getTime();
  if (Number.isNaN(t)) return '—';
  const now = opts.now == null ? Date.now() : new Date(opts.now).getTime();
  const secs = Math.floor((now - t) / 1000);
  if (secs < 0) return opts.future ? `in ${formatDuration(-secs)}` : 'just now';
  if (secs < 1) return 'just now';
  return `${formatDuration(secs)} ago`;
}

/** Detect the S3 endpoint from the current browser URL (same origin in single-port mode) */
export function detectDefaultEndpoint(): string {
  return window.location.origin;
}

/** Clamp `n` into `[lo, hi]`. Non-finite input collapses to `lo`. */
export function clamp(n: number, lo: number, hi: number): number {
  if (!Number.isFinite(n)) return lo;
  return Math.max(lo, Math.min(hi, n));
}

/** Natural (numeric) string comparator — `v2` < `v10`, `file-1` < `file-20`.
 *  Uses `localeCompare` with `{ numeric: true }`. Shared across all sort sites
 *  so the object table, bucket list, and key list never disagree. */
export function numericCompare(a: string, b: string): number {
  return a.localeCompare(b, undefined, { numeric: true, sensitivity: 'base' });
}
