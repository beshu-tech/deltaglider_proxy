export function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
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

/** Compact relative time from an ISO string against an explicit `now` (so a
 *  caller ticking `now` re-renders live). e.g. "5s ago", "3m ago", "2h ago". */
export function relativeTime(iso: string, now: Date): string {
  const d = Math.floor((now.getTime() - new Date(iso).getTime()) / 1000);
  if (d < 0) return 'just now';
  if (d < 60) return `${d}s ago`;
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  return `${Math.floor(d / 86400)}d ago`;
}

/** Format a date as relative time, e.g. "6 hours ago" */
export function timeAgo(date: Date): string {
  const seconds = Math.floor((Date.now() - date.getTime()) / 1000);
  if (seconds < 60) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return `${months}mo ago`;
  const years = Math.floor(days / 365);
  return `${years}y ago`;
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

/**
 * Tiny 6×6 dot pattern as a CSS background-image (inline SVG, no
 * external asset). Used to texturise "saved" bars so they read as
 * negative space without introducing another colour to the eye.
 */
export function dotPattern(color: string): string {
  const safe = encodeURIComponent(color);
  const svg = `<svg xmlns='http://www.w3.org/2000/svg' width='6' height='6'><circle cx='1' cy='1' r='1' fill='${safe}' fill-opacity='0.5'/></svg>`;
  return `url("data:image/svg+xml;utf8,${svg.replace(/"/g, "'")}")`;
}

/** Natural (numeric) string comparator — `v2` < `v10`, `file-1` < `file-20`.
 *  Uses `localeCompare` with `{ numeric: true }`. Shared across all sort sites
 *  so the object table, bucket list, and key list never disagree. */
export function numericCompare(a: string, b: string): number {
  return a.localeCompare(b, undefined, { numeric: true, sensitivity: 'base' });
}

/** "3h 21m ago" / "47s ago" / "never" — coarse age label for cache timestamps. */
export function ageLabel(iso: string | null): string {
  if (!iso) return 'never';
  const ms = Date.now() - new Date(iso).getTime();
  if (ms < 0) return 'just now';
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) {
    const mm = m % 60;
    return mm ? `${h}h ${mm}m ago` : `${h}h ago`;
  }
  const d = Math.floor(h / 24);
  return `${d}d ago`;
}
