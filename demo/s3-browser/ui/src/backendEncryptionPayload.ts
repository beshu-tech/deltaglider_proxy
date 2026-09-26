/**
 * Pure storage-section PUT payload builder for per-backend encryption
 * changes (extracted from BackendsPanel.handleEncryptionApply).
 *
 * Lives in its own React/antd-free module so the unit test
 * can transpile-and-import it and assert the wire body byte-for-byte —
 * this is the one genuinely-pure decision point in BackendsPanel, and
 * the composed body is exactly what the admin API receives, so it must
 * never drift.
 *
 * Composes a `storage` section-PUT body that mutates ONLY the target
 * backend's `encryption` block and leaves every sibling backend + every
 * non-encryption field untouched. The server's RFC 7396 merge-patch
 * semantics guarantee siblings are preserved — we just need to send the
 * correct shape for the path we want to replace.
 *
 * Path:
 *   * Singleton (synthetic "default" backend surfaced by the server when
 *     `backends` is empty) → `{ backend_encryption: <patch> }`.
 *   * Named entry (any other name) → `{ backends: [{name, type, ...,
 *     encryption}] }`. The server replaces `backends` as a whole array
 *     on a section PUT; for per-entry edits we send the FULL list with
 *     only the target entry's `encryption` swapped.
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

/** The subset of `BackendInfo` this builder reads. Matches the live
 *  `BackendInfo` shape (extra fields ignored) so callers pass it as-is. */
interface BackendShapeSource {
  name: string;
  backend_type: string;
  path?: string | null;
  endpoint?: string | null;
  region?: string | null;
  force_path_style?: boolean | null;
}

/** Translate the per-mode patch into the wire `encryption` block.
 *
 *  null-clears for `legacy_key` pass through; absent fields rely on the
 *  server's three-state preservation to keep the previous value.
 *
 *  Internal helper — the only public surface is
 *  `buildEncryptionSectionBody`; this is exercised through it. */
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
 * `backendName`. Byte-identical to the body BackendsPanel composed
 * inline before the useSectionEditor migration.
 */
export function buildEncryptionSectionBody(
  backendName: string,
  patch: EncryptionPatch,
  backends: BackendShapeSource[],
): Record<string, unknown> {
  const encBody = encryptionBody(patch);

  // The singleton ("default") path and the named-entries path have
  // different shapes on disk.
  if (backendName === 'default' && backends.length === 1 && backends[0].name === 'default') {
    // Legacy singleton path — synthesise the singleton
    // `backend_encryption` block. The server handles the preservation
    // for us.
    return { backend_encryption: encBody };
  }

  // Named-backend path: replace the whole list with the edited
  // encryption entry. The server's `preserve_backend_secrets` keeps
  // non-encryption fields intact (e.g. S3 creds); the
  // `preserve_backend_encryption_secrets` walker preserves sibling
  // fields inside the encryption block itself.
  const list = backends.map((b) => {
    const backendShape: Record<string, unknown> = {
      name: b.name,
      type: b.backend_type,
    };
    if (b.path) backendShape.path = b.path;
    if (b.endpoint) backendShape.endpoint = b.endpoint;
    if (b.region) backendShape.region = b.region;
    if (b.force_path_style !== null && b.force_path_style !== undefined) {
      backendShape.force_path_style = b.force_path_style;
    }
    if (b.name === backendName) {
      backendShape.encryption = encBody;
    }
    return backendShape;
  });
  return { backends: list };
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
