import type { S3Object } from './types';

export function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return `${(bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
}

export function savingsPercent(obj: S3Object): number | null {
  if (!obj.storedSize || obj.size === 0) return null;
  return Math.max(0, (1 - obj.storedSize / obj.size) * 100);
}

export function badgeClass(type?: string): string {
  if (!type) return 'badge';
  return `badge badge-${type}`;
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
