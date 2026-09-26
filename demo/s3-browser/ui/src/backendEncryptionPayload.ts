/**
 * Pure storage-section PUT payload builder for per-backend encryption
 * changes (extracted from BackendsPanel.handleEncryptionApply).
 *
 * Lives in its own React/antd-free module so the unit test can import it
 * and assert the wire body exactly: the composed body is what the admin
 * API receives.
 *
 * Path:
 *   * Singleton (the storage section has no `backends` list; the server
 *     surfaces the synthetic "default" backend) →
 *     `{ backend_encryption: <patch> }`, merged by RFC 7396 onto the
 *     running block.
 *   * Named entry → `{ backends: [...] }`. A section PUT replaces the
 *     `backends` array as a whole, so the list is the storage section's
 *     OWN list (from its GET), every entry copied as-is, and only the
 *     target's `encryption` swapped. A list rebuilt from the summary
 *     `BackendInfo` dropped every field it does not carry (`allow_local`,
 *     the siblings' `encryption`), and the server applied that loss.
 */

/** Mirror of `BackendEncryptionPatch` in BackendEncryptionEditor, kept
 *  React-free here so this module has no component imports. */
interface EncryptionPatch {
  mode: string;
  key?: string;
  key_id?: string | null;
  kms_key_id?: string;
  bucket_key_enabled?: boolean;
  legacy_key?: string | null;
  legacy_key_id?: string | null;
}

/** The part of the storage section GET body this builder reads. Each
 *  entry is passed through untouched, so its fields are opaque here. */
export interface StorageSectionBackends {
  backends?: Array<Record<string, unknown>>;
}

/** Translate the per-mode patch into the wire `encryption` block.
 *
 *  null-clears pass through; absent fields rely on the server's
 *  three-state preservation to keep the previous value. */
function encryptionBody(patch: EncryptionPatch): Record<string, unknown> {
  const encBody: Record<string, unknown> = { mode: patch.mode };
  if (patch.key !== undefined) encBody.key = patch.key;
  if (patch.key_id !== undefined) encBody.key_id = patch.key_id;
  if (patch.kms_key_id !== undefined) encBody.kms_key_id = patch.kms_key_id;
  if (patch.bucket_key_enabled !== undefined) encBody.bucket_key_enabled = patch.bucket_key_enabled;
  if (patch.legacy_key !== undefined) encBody.legacy_key = patch.legacy_key;
  if (patch.legacy_key_id !== undefined) encBody.legacy_key_id = patch.legacy_key_id;
  return encBody;
}

/**
 * Build the `storage` section-PUT payload for an encryption change on
 * `backendName`, from the current storage section (its GET body).
 * Throws when a named target is not in the section (a stale page).
 */
export function buildEncryptionSectionBody(
  backendName: string,
  patch: EncryptionPatch,
  storage: StorageSectionBackends,
): Record<string, unknown> {
  const encBody = encryptionBody(patch);
  const list = storage.backends ?? [];
  if (list.length === 0 && backendName === 'default') {
    return { backend_encryption: encBody };
  }
  if (!list.some((b) => b.name === backendName)) {
    throw new Error(`Backend "${backendName}" is not in the storage configuration. Reload the page.`);
  }
  return {
    backends: list.map((b) => (b.name === backendName ? { ...b, encryption: encBody } : b)),
  };
}

/**
 * The patch for a new proxy-AES key. On a backend that already runs
 * proxy-AES this is a rotation: the UI never sees the current key (GET
 * redacts it), so it names the key it retires by id (`legacy_key_id`)
 * and the server moves that key into the legacy slot. `key_id: null`
 * drops an explicit id, so the new key gets its own derived id; an id
 * shared by both keys would make old objects decrypt with the new key.
 */
export function aesKeyPatch(
  key: string,
  current: { mode: string; key_id?: string },
): EncryptionPatch & { mode: 'aes256-gcm-proxy' } {
  if (current.mode === 'aes256-gcm-proxy' && current.key_id) {
    return { mode: 'aes256-gcm-proxy', key, key_id: null, legacy_key_id: current.key_id };
  }
  return { mode: 'aes256-gcm-proxy', key };
}
