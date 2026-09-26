import type { BackendInfo } from './adminApi';

/** One-line description of a backend for the Backends list. The access key
 *  id is an identifier (the server never returns the secret), shown so the
 *  operator sees which key a backend uses. */
export function backendSummary(
  b: Pick<BackendInfo, 'backend_type' | 'path' | 'endpoint' | 'region' | 'access_key_id'>,
): string {
  if (b.backend_type === 'filesystem') return `filesystem: ${b.path}`;
  const base = `s3: ${b.endpoint || 'AWS'} (${b.region})`;
  return b.access_key_id ? `${base} · key ${b.access_key_id}` : base;
}
