// ─────────────────────────────────────────────────────────────
// Encryption witness (issue #116 P3 — several instances)
// ─────────────────────────────────────────────────────────────
import { adminJson, confirmHeaderMap } from './core';
import type { WitnessStatus } from '../encryptionWitness';

/** The witness verdict of this instance; `null` on a single instance. */
export async function getEncryptionWitness(): Promise<WitnessStatus | null> {
  return adminJson('/api/admin/encryption-witness', { context: 'Encryption witness' });
}

/**
 * Publish this instance's encryption of `storages` (bucket names,
 * `backend:<name>`, `route:default`). A subject that this instance stores
 * in plaintext must be in `downgrades` (header
 * `X-DGP-Confirm-Encryption-Downgrade`), else the server answers 400.
 */
export async function publishEncryptionWitness(
  storages: string[],
  downgrades: string[],
): Promise<{ status: string }> {
  return adminJson('/api/admin/encryption-witness/publish', {
    method: 'POST',
    body: { storages },
    headers: confirmHeaderMap({ downgrades }),
    context: 'Publish encryption',
  });
}
