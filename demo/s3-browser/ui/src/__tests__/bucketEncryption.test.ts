/**
 * Issue #116 P2: the pure half of per-bucket encryption in the UI. Every
 * answer starts from the server's `effective_encryption`.
 */
import { describe, expect, test } from 'vitest';
import type { EffectiveEncryption } from '../adminApi';
import {
  backendBucketCountText,
  backendBucketCounts,
  bucketEncryptionBody,
  bucketKeyYaml,
  bucketsWhoseEncryptionChanged,
  choiceBlocked,
  confirmFor,
  currentChoice,
  describeBucketEncryption,
  encryptsAfterMigrate,
  inheritLabel,
  isAesKeyHex,
  isDowngrade,
  keyFileText,
  keyTailMatches,
  offeredChoices,
  reencryptTransition,
  suggestKeyId,
} from '../bucketEncryption';

const KEY = 'ab'.repeat(29) + 'c0ffee';

function eff(over: Partial<EffectiveEncryption> = {}): EffectiveEncryption {
  return {
    mode: 'none',
    source: 'backend',
    backend: 'hetzner-fsn1',
    has_key: false,
    encrypts: false,
    reencrypt: false,
    pinned: true,
    bucket_modes: ['none', 'aes256-gcm-proxy'],
    inherited: { mode: 'none', encrypts: false },
    ...over,
  };
}
const bucketKey = eff({
  mode: 'aes256-gcm-proxy',
  source: 'bucket',
  has_key: true,
  encrypts: true,
  reencrypt: true,
  key_id: 'db-archive-2026-10',
});

describe('the collapsed chip names where the encryption comes from', () => {
  test('inherited from the backend', () => {
    const d = describeBucketEncryption(
      eff({ mode: 'aes256-gcm-proxy', has_key: true, encrypts: true, key_id: 'k1' }),
    );
    expect(d.label).toBe('Encrypted · from hetzner-fsn1');
    expect(d.isEncrypted).toBe(true);
  });
  test("the bucket's own key", () => {
    expect(describeBucketEncryption(bucketKey).label).toBe('Encrypted · bucket key db-archive-2026-10');
  });
  test('off for this bucket is not the same as a plaintext backend', () => {
    expect(describeBucketEncryption(eff({ source: 'bucket' })).label).toBe('Not encrypted · off for this bucket');
    expect(describeBucketEncryption(eff()).label).toBe('Not encrypted');
  });
  test('a proxy mode without a key says that writes are refused', () => {
    expect(describeBucketEncryption(eff({ mode: 'aes256-gcm-proxy' })).label).toBe('No key · writes refused');
  });
  test('no answer', () => {
    expect(describeBucketEncryption(null).label).toBe('Encryption unknown');
  });
});

test('the radio shows the modes that the server accepts on a bucket', () => {
  expect(offeredChoices(eff())).toEqual(['inherit', 'bucket-key', 'off']);
  expect(offeredChoices(eff({ bucket_modes: ['none', 'aes256-gcm-proxy', 'sse-kms', 'sse-s3'] }))).toEqual([
    'inherit',
    'bucket-key',
    'sse-kms',
    'sse-s3',
    'off',
  ]);
  expect(currentChoice(eff())).toBe('inherit');
  expect(currentChoice(bucketKey)).toBe('bucket-key');
  expect(currentChoice(eff({ source: 'bucket' }))).toBe('off');
  expect(inheritLabel(eff({ inherited: { mode: 'aes256-gcm-proxy', encrypts: true } }))).toBe(
    'Inherit — hetzner-fsn1: proxy AES-256-GCM',
  );
});

describe('the wire body is one bucket merge-patch', () => {
  test('a bucket key on an unpinned bucket pins the backend that holds it', () => {
    expect(
      bucketEncryptionBody('db-archive', eff({ pinned: false }), { kind: 'bucket-key', key: KEY, keyId: 'db-archive-2026-10' }),
    ).toEqual({
      buckets: {
        'db-archive': {
          backend: 'hetzner-fsn1',
          // No read policy: the server converts the history (#120).
          encryption: { mode: 'aes256-gcm-proxy', key: KEY, key_id: 'db-archive-2026-10' },
        },
      },
    });
  });
  test('a pinned bucket keeps its route untouched', () => {
    expect(bucketEncryptionBody('db-archive', bucketKey, { kind: 'off' })).toEqual({
      buckets: { 'db-archive': { encryption: { mode: 'none' } } },
    });
  });
  test('inherit removes the block and never pins', () => {
    expect(bucketEncryptionBody('db-archive', eff({ source: 'bucket', pinned: false }), { kind: 'inherit' })).toEqual({
      buckets: { 'db-archive': { encryption: null } },
    });
  });
  test('clear legacy nulls only the two legacy fields', () => {
    expect(bucketEncryptionBody('db-archive', bucketKey, { kind: 'clear-legacy' })).toEqual({
      buckets: { 'db-archive': { encryption: { legacy_key: null, legacy_key_id: null } } },
    });
  });
});

describe('confirmation headers', () => {
  test('off on an encrypted bucket is a downgrade', () => {
    expect(isDowngrade(bucketKey, { kind: 'off' })).toBe(true);
    expect(confirmFor('db-archive', bucketKey, { kind: 'off' })).toEqual({ downgrades: ['db-archive'] });
  });
  test('inherit onto a plaintext backend is a downgrade; onto an encrypting one it is not', () => {
    const own = { ...bucketKey, has_key: false, mode: 'sse-s3' as const };
    expect(isDowngrade(own, { kind: 'inherit' })).toBe(true);
    expect(isDowngrade({ ...own, inherited: { mode: 'sse-s3', encrypts: true } }, { kind: 'inherit' })).toBe(false);
  });
  test('a new key is no downgrade', () => {
    expect(confirmFor('db-archive', bucketKey, { kind: 'bucket-key', key: KEY, keyId: 'x' })).toEqual({});
  });
  test('clear legacy confirms the legacy key id', () => {
    const e = { ...bucketKey, legacy_key_id: 'db-archive-2026-04' };
    expect(confirmFor('db-archive', e, { kind: 'clear-legacy' })).toEqual({ keyRemovals: ['db-archive-2026-04'] });
  });
});

describe('choices that the server would refuse are blocked with the reason', () => {
  test('inherit while the bucket holds a key', () => {
    expect(choiceBlocked(bucketKey, 'inherit')).toMatch(/Off for this bucket/);
    expect(choiceBlocked(eff({ source: 'bucket', legacy_key_id: 'old' }), 'inherit')).toMatch(/legacy key/);
    expect(choiceBlocked(eff({ source: 'bucket' }), 'inherit')).toBeNull();
  });
  test('a second retirement while the legacy slot is full', () => {
    const full = { ...bucketKey, legacy_key_id: 'old' };
    expect(choiceBlocked(full, 'off')).toMatch(/legacy key slot/);
    expect(choiceBlocked(bucketKey, 'off')).toBeNull();
  });
  test('an env-sourced key is read-only', () => {
    expect(choiceBlocked({ ...bucketKey, key_env: 'DB_ARCHIVE_KEY' }, 'off')).toMatch(/DB_ARCHIVE_KEY/);
  });
});

test('key helpers', () => {
  expect(suggestKeyId('db-archive', new Date(Date.UTC(2026, 9, 14)))).toBe('db-archive-2026-10');
  expect(suggestKeyId('x'.repeat(63), new Date(Date.UTC(2026, 0, 1)))).toHaveLength(64);
  expect(isAesKeyHex(KEY)).toBe(true);
  expect(isAesKeyHex(KEY.slice(1))).toBe(false);
  expect(keyTailMatches(KEY, KEY.slice(-6).toUpperCase())).toBe(true);
  expect(keyTailMatches(KEY, '000000')).toBe(false);
  const file = keyFileText({ bucket: 'db-archive', backend: 'hetzner-fsn1', keyId: 'k', key: KEY, now: new Date(0) });
  expect(file).toContain(`key: ${KEY}`);
  expect(file).toContain('key_id: k');
  expect(file).toContain('1970-01-01');
  expect(file).toMatch(/Recovery/);
  expect(bucketKeyYaml({ bucket: 'db-archive', backend: 'hetzner-fsn1', keyId: 'k', envVar: 'DB_ARCHIVE_ENCRYPTION_KEY' })).toContain(
    'key: ${env:DB_ARCHIVE_ENCRYPTION_KEY}',
  );
});

describe('re-encrypt proposals are per bucket', () => {
  test('transition words', () => {
    expect(reencryptTransition(eff(), bucketKey)).toBe('encrypt');
    expect(reencryptTransition(bucketKey, { ...bucketKey, key_id: 'new' })).toBe('rotate');
    expect(reencryptTransition(bucketKey, bucketKey)).toBeNull();
    expect(reencryptTransition(bucketKey, eff({ source: 'bucket', reencrypt: true, legacy_key_id: 'x' }))).toBe('decrypt');
    // Nothing for the job to do: no proposal.
    expect(reencryptTransition(bucketKey, eff({ source: 'bucket', reencrypt: false }))).toBeNull();
  });
  test('only buckets whose effective encryption changed are preselected', () => {
    const before = [
      { name: 'releases', effective_encryption: eff() },
      { name: 'db-archive', effective_encryption: bucketKey },
    ];
    const after = [
      { name: 'releases', effective_encryption: eff({ mode: 'aes256-gcm-proxy', has_key: true, encrypts: true, reencrypt: true, key_id: 'k1' }) },
      // An override: the backend change does not touch it.
      { name: 'db-archive', effective_encryption: bucketKey },
    ];
    expect(bucketsWhoseEncryptionChanged(before, after)).toEqual([{ bucket: 'releases', transition: 'encrypt' }]);
  });
});

test('migrate: a bucket key moves with the bucket; otherwise the target decides', () => {
  const plain = { mode: 'none' as const, has_key: false, shim_active: false };
  expect(encryptsAfterMigrate(bucketKey, plain)).toBe(true);
  expect(encryptsAfterMigrate(eff({ encrypts: true, mode: 'sse-s3' }), plain)).toBe(false);
  expect(encryptsAfterMigrate(eff(), { mode: 'aes256-gcm-proxy', has_key: true, shim_active: false })).toBe(true);
});

test('backend card counts', () => {
  const c = backendBucketCounts([
    { effective_encryption: eff() },
    { effective_encryption: eff() },
    { effective_encryption: bucketKey },
    { effective_encryption: eff({ backend: 'local-disk' }) },
    { effective_encryption: null },
  ]);
  expect(c).toEqual({ 'hetzner-fsn1': { inherit: 2, override: 1 }, 'local-disk': { inherit: 1, override: 0 } });
  expect(backendBucketCountText(c['hetzner-fsn1'])).toBe('Applies to 2 buckets · 1 bucket has its own setting');
  expect(backendBucketCountText(c['local-disk'])).toBe('Applies to 1 bucket');
  expect(backendBucketCountText(undefined)).toBeNull();
});
