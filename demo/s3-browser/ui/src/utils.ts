export function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
}

/** Extract the display name from a full S3 key given the current prefix */
export function displayName(key: string, prefix: string): string {
  return key.startsWith(prefix) ? key.slice(prefix.length) : key;
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

/** Detect the S3 endpoint from the current browser URL (UI port - 1, or 9000 for Vite dev) */
export function detectDefaultEndpoint(): string {
  if (typeof window !== 'undefined') {
    const port = parseInt(window.location.port, 10);
    if (port) {
      // Vite dev server runs on 5173; proxy is on 9000
      if (port === 5173) return `${window.location.protocol}//${window.location.hostname}:9000`;
      return `${window.location.protocol}//${window.location.hostname}:${port - 1}`;
    }
  }
  return 'http://localhost:9000';
}
