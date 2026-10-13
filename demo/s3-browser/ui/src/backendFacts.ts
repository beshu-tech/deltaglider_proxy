/**
 * Pure backend facts for the job dialogs (migrate, re-encrypt, backfill)
 * and the bucket cards: which backend holds a bucket, whether the backend
 * encrypts at rest, and whether the health loop gates it.
 */
import type { BackendEncryptionSummary, BackendHealthEntry, BackendInfo } from './adminApi';

/** The backend that holds a bucket: the origins `backend_name` (the server
 *  resolves routes, aliases and buckets that exist only on a non-default
 *  backend), else the default backend. */
export function originBackendOf(
  origin: { backend_name?: string | null } | undefined,
  defaultBackend: string | null,
): string | null {
  const name = origin?.backend_name?.trim();
  return name ? name : defaultBackend;
}

/** True when new writes to the backend are encrypted at rest. A proxy-AES
 *  backend without a key writes plaintext. */
export function encryptsAtRest(enc: BackendEncryptionSummary | undefined): boolean {
  if (!enc) return false;
  switch (enc.mode) {
    case 'aes256-gcm-proxy':
      return enc.has_key;
    case 'sse-kms':
    case 'sse-s3':
      return true;
    default:
      return false;
  }
}

/** Short encryption word for a picker row. */
export function encryptionWord(enc: BackendEncryptionSummary | undefined): string {
  if (!enc) return 'encryption unknown';
  switch (enc.mode) {
    case 'aes256-gcm-proxy':
      return enc.has_key ? 'proxy AES-256-GCM' : 'plaintext (no key set)';
    case 'sse-kms':
      return 'SSE-KMS';
    case 'sse-s3':
      return 'SSE-S3';
    default:
      return 'plaintext';
  }
}

/** Short health word for a picker row. No entry = the loop never probed it. */
export function healthWord(h: BackendHealthEntry | undefined): string {
  if (!h) return 'never probed';
  switch (h.status) {
    case 'healthy':
      return 'healthy';
    case 'auth-rejected':
      return 'credentials rejected';
    case 'unreachable':
      return 'unreachable';
    case 'erroring':
      return 'erroring';
  }
}

/** The health loop gates a backend whose credentials are rejected or whose
 *  endpoint does not answer: every request to it fails until it recovers. */
export function isGated(h: BackendHealthEntry | undefined): boolean {
  return h?.status === 'auth-rejected' || h?.status === 'unreachable';
}

/** True when a re-encrypt job can have work on the backend: it encrypts
 *  with the proxy key, or a legacy key is still set (objects under it get
 *  rewritten). A plaintext backend without a legacy key has nothing to do. */
export function reencryptHasWork(b: BackendInfo | undefined): boolean {
  if (!b) return false;
  return (b.encryption.mode === 'aes256-gcm-proxy' && b.encryption.has_key) || b.encryption.shim_active;
}

/** The migrate dialog's label for the mirror option. The server's 409 names
 *  the API field; the dialog says it in its own words. */
export const MIRROR_OPTION_LABEL = 'Make the destination an exact mirror of the source';

/** A migrate start error in the dialog's words. */
export function migrateErrorInModalWords(text: string): string {
  return text
    .replace(/\s*\(API: "target": "mirror"\)/g, '')
    .replace(/"Make the destination an exact mirror"/g, `"${MIRROR_OPTION_LABEL}"`);
}
