/**
 * Per-backend encryption UI helpers.
 *
 * Two small pure functions split out of BucketsPanel.tsx:
 *
 *   * `resolveBackendFor` — given a bucket row's backend override
 *     (possibly empty), a list of backends, and the configured
 *     default, returns the BackendInfo that handles the bucket.
 *   * `describeEncryption` — maps a `BackendEncryptionSummary` to UI
 *     copy (label, tooltip, is-encrypted tone).
 *
 * Kept in a .ts file (not .tsx) with no component exports so the
 * Vite fast-refresh linter doesn't grouse about "this file exports
 * both a component and a helper." Also unit-testable without a DOM.
 */
import type { BackendEncryptionSummary, BackendInfo } from './adminApi';

/**
 * Resolve which backend handles a bucket. Returns the matching
 * `BackendInfo` or null. Resolution order:
 *   1. Explicit `rowBackend` (operator pinned this bucket to a
 *      specific backend name).
 *   2. `defaultBackend` — the YAML's `default_backend` field.
 *   3. Synthetic `"default"` entry — the server surfaces this for
 *      the legacy singleton-backend path so every bucket has a
 *      resolvable target even when no named list is configured.
 */
export function resolveBackendFor(
  rowBackend: string,
  backends: BackendInfo[],
  defaultBackend: string | null,
): BackendInfo | null {
  const explicit = rowBackend.trim();
  if (explicit) {
    const match = backends.find((b) => b.name === explicit);
    if (match) return match;
  }
  if (defaultBackend) {
    const match = backends.find((b) => b.name === defaultBackend);
    if (match) return match;
  }
  return backends.find((b) => b.name === 'default') ?? null;
}

/**
 * Map a `BackendEncryptionSummary` to UI copy. Distinct labels per
 * mode so operators can tell at a glance whether they're looking at
 * proxy-AES, SSE-KMS, SSE-S3, or plaintext.
 *
 * Returns `{ label, tooltip, isEncrypted }` — `isEncrypted` drives
 * the badge's green-vs-muted colour tone.
 */
export function describeEncryption(
  summary: BackendEncryptionSummary | undefined,
): { label: string; tooltip: string; isEncrypted: boolean } {
  if (!summary || summary.mode === 'none') {
    return {
      label: 'Not encrypted',
      tooltip:
        'Objects on this backend are plaintext. Configure encryption on the backend in Admin → Storage → Backends.',
      isEncrypted: false,
    };
  }
  switch (summary.mode) {
    case 'aes256-gcm-proxy':
      return {
        label: 'Encrypted (proxy, AES-256-GCM)',
        tooltip: summary.has_key
          ? `Proxy-side AES-256-GCM. Key id: ${summary.key_id ?? '<unset>'}.${
              summary.shim_active
                ? ' Decrypt-only shim active — legacy_key still configured.'
                : ''
            }`
          : 'Mode is aes256-gcm-proxy but no key is configured. Writes will go to disk as plaintext until a key is set.',
        isEncrypted: summary.has_key,
      };
    case 'sse-kms':
      return {
        label: 'Encrypted (SSE-KMS)',
        tooltip: `AWS KMS-managed encryption. KMS key: ${summary.kms_key_id ?? '<unset>'}.${
          summary.shim_active
            ? ' Decrypt-only shim active — legacy proxy key still configured for historical reads.'
            : ''
        }`,
        isEncrypted: true,
      };
    case 'sse-s3':
      return {
        label: 'Encrypted (SSE-S3)',
        tooltip: `AWS-managed AES256 encryption (SSE-S3).${
          summary.shim_active
            ? ' Decrypt-only shim active — legacy proxy key still configured for historical reads.'
            : ''
        }`,
        isEncrypted: true,
      };
    default:
      return {
        label: 'Not encrypted',
        tooltip: `Unknown encryption mode: ${summary.mode}`,
        isEncrypted: false,
      };
  }
}
