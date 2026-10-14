/**
 * Per-bucket encryption (issue #116, P2): the pure half of the bucket
 * card's Encryption group, the create-bucket choice and the re-encrypt
 * proposals.
 *
 * Every function here starts from the server's `effective_encryption` of a
 * bucket (GET /api/admin/buckets). None of them decides which backend holds
 * a bucket or which encryption applies to it: the server's one resolver
 * does that (`Config::encryption_for_listed_bucket`), and the source guard
 * `noClientEncryptionResolution.test.ts` refuses a client-side copy.
 *
 * The wire body is a storage-section merge-patch (RFC 7396) of ONE bucket:
 * an absent key keeps the server's value, `null` removes it. The server
 * keeps the key material that the GET redacts, and it moves a retired key
 * into the decrypt-only legacy slot when the body leaves `legacy_key` out.
 */
import type {
  BackendEncryptionMode,
  BackendEncryptionSummary,
  ConfirmHeaders,
  EffectiveEncryption,
} from './adminApi';
import { encryptsAtRest } from './backendFacts';

/** The radio of the bucket card. "off" is not "inherit": off writes
 *  plaintext even on an encrypting backend, and keeps the bucket key as a
 *  decrypt-only legacy key. */
export type BucketEncryptionChoice = 'inherit' | 'bucket-key' | 'off' | 'sse-kms' | 'sse-s3';

/** The choice that the bucket's current configuration is. */
export function currentChoice(eff: EffectiveEncryption): BucketEncryptionChoice {
  if (eff.source === 'backend') return 'inherit';
  switch (eff.mode) {
    case 'aes256-gcm-proxy':
      return 'bucket-key';
    case 'sse-kms':
      return 'sse-kms';
    case 'sse-s3':
      return 'sse-s3';
    default:
      return 'off';
  }
}

/** The bucket-block mode of a choice (`inherit` has none). */
const CHOICE_MODE: Record<Exclude<BucketEncryptionChoice, 'inherit'>, BackendEncryptionMode> = {
  'bucket-key': 'aes256-gcm-proxy',
  off: 'none',
  'sse-kms': 'sse-kms',
  'sse-s3': 'sse-s3',
};

/** The choices to show: Inherit always, the others when the server accepts
 *  their mode on a bucket (`bucket_modes`). A mode that the server learns to
 *  accept appears with no change here. */
export function offeredChoices(eff: EffectiveEncryption): BucketEncryptionChoice[] {
  const out: BucketEncryptionChoice[] = ['inherit'];
  for (const c of ['bucket-key', 'sse-kms', 'sse-s3', 'off'] as const) {
    if (eff.bucket_modes.includes(CHOICE_MODE[c])) out.push(c);
  }
  return out;
}

/** A short word for a mode. */
export function modeWord(mode: BackendEncryptionMode, encrypts = true): string {
  switch (mode) {
    case 'aes256-gcm-proxy':
      return encrypts ? 'proxy AES-256-GCM' : 'proxy AES-256-GCM, no key';
    case 'sse-kms':
      return 'SSE-KMS';
    case 'sse-s3':
      return 'SSE-S3';
    default:
      return 'plaintext';
  }
}

/** The collapsed chip of a bucket: label with provenance, tooltip, tone. */
export function describeBucketEncryption(eff: EffectiveEncryption | null | undefined): {
  label: string;
  tooltip: string;
  isEncrypted: boolean;
} {
  if (!eff) {
    return {
      label: 'Encryption unknown',
      tooltip: 'The proxy could not tell how this bucket is encrypted: it routes to a backend that is not defined.',
      isEncrypted: false,
    };
  }
  const legacy = eff.legacy_key_id
    ? ` Objects written under the legacy key id ${eff.legacy_key_id} stay readable.`
    : '';
  if (eff.mode === 'aes256-gcm-proxy' && !eff.encrypts) {
    return {
      label: 'No key · writes refused',
      tooltip: `The ${eff.source === 'bucket' ? 'bucket' : `backend ${eff.backend}`} is set to encrypt with a proxy key, but no key is configured. The proxy refuses every write to this bucket; it never stores plaintext there.`,
      isEncrypted: false,
    };
  }
  if (eff.encrypts) {
    if (eff.source === 'bucket') {
      const what = eff.mode === 'aes256-gcm-proxy' ? `bucket key ${eff.key_id ?? ''}`.trim() : `${modeWord(eff.mode)} (bucket)`;
      return {
        label: `Encrypted · ${what}`,
        tooltip: `This bucket has its own encryption: ${modeWord(eff.mode)}${eff.key_id ? `, key id ${eff.key_id}` : ''}${eff.kms_key_id ? `, KMS key ${eff.kms_key_id}` : ''}. Other buckets on ${eff.backend} are not affected.${legacy}`,
        isEncrypted: true,
      };
    }
    return {
      label: `Encrypted · from ${eff.backend}`,
      tooltip: `The bucket inherits the encryption of backend ${eff.backend}: ${modeWord(eff.mode)}${eff.key_id ? `, key id ${eff.key_id}` : ''}${eff.kms_key_id ? `, KMS key ${eff.kms_key_id}` : ''}.${legacy}`,
      isEncrypted: true,
    };
  }
  if (eff.source === 'bucket') {
    return {
      label: 'Not encrypted · off for this bucket',
      tooltip: `Encryption is off for this bucket: new writes are stored as plaintext, whatever backend ${eff.backend} does.${legacy}`,
      isEncrypted: false,
    };
  }
  return {
    label: 'Not encrypted',
    tooltip: `Backend ${eff.backend} stores objects as plaintext. Choose "Encrypt with a bucket key" in the bucket's Encryption group to encrypt only this bucket.${legacy}`,
    isEncrypted: false,
  };
}

/** The label of the Inherit radio: "Inherit — hetzner-fsn1: plaintext". */
export function inheritLabel(eff: EffectiveEncryption): string {
  return `Inherit — ${eff.backend}: ${modeWord(eff.inherited.mode, eff.inherited.encrypts)}`;
}

/** The pattern that the server accepts for a key id. */
const KEY_ID_RE = /^[A-Za-z0-9_.-]{1,64}$/;

export function isValidKeyId(id: string): boolean {
  return KEY_ID_RE.test(id);
}

/** A key id suggestion: `<bucket>-<yyyy>-<mm>` (the month of the key). */
export function suggestKeyId(bucket: string, now: Date = new Date()): string {
  const mm = String(now.getUTCMonth() + 1).padStart(2, '0');
  const suffix = `-${now.getUTCFullYear()}-${mm}`;
  return `${bucket.slice(0, 64 - suffix.length)}${suffix}`;
}

/** 64 hexadecimal characters: the form `EncryptionKey::from_hex` accepts. */
export function isAesKeyHex(key: string): boolean {
  return /^[0-9a-fA-F]{64}$/.test(key);
}

/** The characters that the "I stored the key" check asks for. */
function keyTail(key: string): string {
  return key.slice(-6);
}

/** True when `typed` proves that the operator holds a copy of `key`. */
export function keyTailMatches(key: string, typed: string): boolean {
  return key.length >= 6 && typed.trim().toLowerCase() === keyTail(key).toLowerCase();
}

/** A suggested environment variable name for a bucket key. */
export function suggestKeyEnvVar(bucket: string): string {
  return `${bucket.toUpperCase().replace(/[^A-Z0-9]+/g, '_')}_ENCRYPTION_KEY`;
}

/** The YAML that keeps the key out of the config file. */
export function bucketKeyYaml(opts: { bucket: string; backend: string; keyId: string; envVar: string }): string {
  return [
    'storage:',
    '  buckets:',
    `    ${opts.bucket}:`,
    `      backend: ${opts.backend}`,
    '      encryption:',
    '        mode: aes256-gcm-proxy',
    `        key_id: ${opts.keyId}`,
    `        key: \${env:${opts.envVar}}`,
    '',
  ].join('\n');
}

/** The text of the downloaded key file. */
export function keyFileText(opts: {
  bucket: string;
  backend: string;
  keyId: string;
  key: string;
  now?: Date;
}): string {
  const date = (opts.now ?? new Date()).toISOString();
  return [
    '# DeltaGlider Proxy bucket encryption key',
    `bucket: ${opts.bucket}`,
    `backend: ${opts.backend}`,
    `key_id: ${opts.keyId}`,
    `created: ${date}`,
    `key: ${opts.key}`,
    '',
    '# Recovery: objects of this bucket that carry this key id can be read only',
    '# with this key. Without it they are lost. Keep this file in a password',
    '# manager or a secrets vault, outside the proxy host. To restore, put the',
    "# key back in the bucket's encryption block with the same key_id:",
    '#   storage.buckets.<bucket>.encryption: { mode: aes256-gcm-proxy, key_id: <key_id>, key: <key> }',
    '',
  ].join('\n');
}

/** The file name of the downloaded key file. */
export function keyFileName(bucket: string, keyId: string): string {
  return `${bucket}.${keyId}.deltaglider-key.txt`;
}

/** One change to a bucket's own encryption. */
export type BucketEncryptionChange =
  | { kind: 'inherit' }
  | { kind: 'off' }
  | { kind: 'bucket-key'; key: string; keyId: string }
  | { kind: 'sse-kms'; kmsKeyId: string; bucketKeyEnabled: boolean }
  | { kind: 'sse-s3' }
  | { kind: 'clear-legacy' };

/** One bucket's merge-patch for a change. */
export function bucketEncryptionBody(
  bucket: string,
  eff: EffectiveEncryption,
  change: BucketEncryptionChange,
): { buckets: Record<string, Record<string, unknown>> } {
  let encryption: Record<string, unknown> | null;
  switch (change.kind) {
    case 'inherit':
      encryption = null;
      break;
    case 'off':
      encryption = { mode: 'none' };
      break;
    case 'bucket-key':
      encryption = { mode: 'aes256-gcm-proxy', key: change.key, key_id: change.keyId };
      break;
    case 'sse-kms':
      encryption = { mode: 'sse-kms', kms_key_id: change.kmsKeyId, bucket_key_enabled: change.bucketKeyEnabled };
      break;
    case 'sse-s3':
      encryption = { mode: 'sse-s3' };
      break;
    case 'clear-legacy':
      encryption = { legacy_key: null, legacy_key_id: null };
      break;
  }
  const policy: Record<string, unknown> = { encryption };
  // A bucket's own block needs a fixed backend: pin the one that holds the
  // bucket now (the server's answer), so the data stays where it is.
  if (encryption !== null && !eff.pinned) policy.backend = eff.backend;
  return { buckets: { [bucket]: policy } };
}

/** Whether new writes are encrypted after the change. */
function encryptsAfter(eff: EffectiveEncryption, change: BucketEncryptionChange): boolean {
  switch (change.kind) {
    case 'inherit':
      return eff.inherited.encrypts;
    case 'off':
      return false;
    case 'clear-legacy':
      return eff.encrypts;
    default:
      return true;
  }
}

/** A change after which new writes of a bucket that is set to encrypt are
 *  plaintext (also a proxy mode without a key, whose writes are refused
 *  today: the server asks for the confirmation there too). */
export function isDowngrade(eff: EffectiveEncryption, change: BucketEncryptionChange): boolean {
  return eff.mode !== 'none' && !encryptsAfter(eff, change);
}

/** The confirmation headers that a change needs. */
export function confirmFor(
  bucket: string,
  eff: EffectiveEncryption,
  change: BucketEncryptionChange,
): ConfirmHeaders {
  const out: ConfirmHeaders = {};
  if (isDowngrade(eff, change)) out.downgrades = [bucket];
  if (change.kind === 'clear-legacy' && eff.legacy_key_id) out.keyRemovals = [eff.legacy_key_id];
  return out;
}

/** Whether a choice can be picked now, and why not. */
export function choiceBlocked(eff: EffectiveEncryption, choice: BucketEncryptionChoice): string | null {
  if (eff.key_env) {
    return `The bucket key comes from the environment variable ${eff.key_env}. Change it in the deployment, not here.`;
  }
  const own = eff.source === 'bucket';
  const hasOwnKey = own && eff.mode === 'aes256-gcm-proxy' && eff.has_key;
  const ownLegacy = own && !!eff.legacy_key_id;
  if (choice === currentChoice(eff)) return null;
  if (choice === 'inherit' && own && (hasOwnKey || ownLegacy)) {
    return hasOwnKey
      ? 'The bucket key must stay readable while objects use it. Choose "Off for this bucket" first (the key becomes a decrypt-only legacy key), re-encrypt the bucket, and clear the legacy key. Then the bucket can inherit.'
      : 'The bucket still holds a legacy key. Re-encrypt the bucket and clear the legacy key first. Then the bucket can inherit.';
  }
  // Leaving the bucket key moves it to the legacy slot; one slot only.
  if (hasOwnKey && ownLegacy) {
    return 'The legacy key slot still holds an older key. Re-encrypt the bucket and clear the legacy key before you change the key or the mode.';
  }
  return null;
}

/** The re-encrypt proposal's wording for a before → after change of one bucket. */
export type ReencryptTransition = 'encrypt' | 'rotate' | 'decrypt';

export function reencryptTransition(
  before: EffectiveEncryption | null | undefined,
  after: EffectiveEncryption | null | undefined,
): ReencryptTransition | null {
  if (!before || !after || !after.reencrypt) return null;
  if (after.encrypts && after.mode === 'aes256-gcm-proxy') {
    if (!before.encrypts) return 'encrypt';
    return before.mode !== after.mode || before.key_id !== after.key_id ? 'rotate' : null;
  }
  if (!after.encrypts && before.encrypts) return 'decrypt';
  return null;
}

/** The buckets whose new-write encryption changed between two listings and
 *  that a re-encrypt job can rewrite: the post-apply proposal preselects
 *  only these. */
export function bucketsWhoseEncryptionChanged(
  before: Array<{ name: string; effective_encryption?: EffectiveEncryption | null }>,
  after: Array<{ name: string; effective_encryption?: EffectiveEncryption | null }>,
): Array<{ bucket: string; transition: ReencryptTransition }> {
  const old = new Map(before.map((o) => [o.name, o.effective_encryption]));
  const out: Array<{ bucket: string; transition: ReencryptTransition }> = [];
  for (const o of after) {
    const t = reencryptTransition(old.get(o.name), o.effective_encryption);
    if (t) out.push({ bucket: o.name, transition: t });
  }
  return out.sort((a, b) => a.bucket.localeCompare(b.bucket));
}

/** One transition word for a set of changes (the modal's title). */
export function dominantTransition(changes: Array<{ transition: ReencryptTransition }>): ReencryptTransition {
  const ts = new Set(changes.map((c) => c.transition));
  if (ts.size === 1) return changes[0].transition;
  return ts.has('encrypt') ? 'encrypt' : ts.has('rotate') ? 'rotate' : 'decrypt';
}

/** Whether new writes stay encrypted when the bucket moves to `target`:
 *  a bucket's own block moves with the bucket; otherwise the target
 *  backend's own encryption applies. */
export function encryptsAfterMigrate(
  eff: EffectiveEncryption | null | undefined,
  target: BackendEncryptionSummary | undefined,
): boolean {
  if (eff?.source === 'bucket') return eff.encrypts;
  return encryptsAtRest(target);
}

/** Per backend: buckets that take its encryption, and buckets that set their own. */
export function backendBucketCounts(
  origins: Array<{ effective_encryption?: EffectiveEncryption | null }>,
): Record<string, { inherit: number; override: number }> {
  const out: Record<string, { inherit: number; override: number }> = {};
  for (const o of origins) {
    const e = o.effective_encryption;
    if (!e) continue;
    const c = (out[e.backend] ??= { inherit: 0, override: 0 });
    if (e.source === 'bucket') c.override += 1;
    else c.inherit += 1;
  }
  return out;
}

/** "Default for 3 buckets · 1 bucket overrides". */
export function backendBucketCountText(c: { inherit: number; override: number } | undefined): string | null {
  if (!c || (c.inherit === 0 && c.override === 0)) return null;
  const n = (k: number, one: string, many: string) => `${k} ${k === 1 ? one : many}`;
  const parts = [`Default for ${n(c.inherit, 'bucket', 'buckets')}`];
  if (c.override > 0) parts.push(`${n(c.override, 'bucket overrides', 'buckets override')}`);
  return parts.join(' · ');
}

/** The encryption choice of the create-bucket dialog. */
export type NewBucketEncryption =
  | { kind: 'same' }
  | { kind: 'bucket-key'; key: string; keyId: string }
  | { kind: 'off' };

/**
 * The `encryption` block that the create-bucket dialog sends WITH the
 * create (POST /api/admin/buckets, one server write), so the first object
 * is already written with it. `undefined` for "Same as backend".
 */
export function newBucketEncryptionBlock(enc: NewBucketEncryption): Record<string, unknown> | undefined {
  if (enc.kind === 'same') return undefined;
  return enc.kind === 'off' ? { mode: 'none' } : { mode: 'aes256-gcm-proxy', key: enc.key, key_id: enc.keyId };
}
