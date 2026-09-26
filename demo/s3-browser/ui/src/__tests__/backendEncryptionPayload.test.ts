/** src/backendEncryptionPayload.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { aesKeyPatch, buildEncryptionSectionBody } from '../backendEncryptionPayload';

// One entry per backend variant, as the storage section GET returns it
// (secrets redacted, so absent). Every field must survive an encryption
// edit on ANOTHER backend untouched.
const variants: Array<Record<string, unknown>> = [
  { name: 'local-disk', type: 'filesystem', path: '/srv/data' },
  {
    name: 'local-enc',
    type: 'filesystem',
    path: '/srv/enc',
    encryption: { mode: 'aes256-gcm-proxy', key_id: 'k-2026', legacy_key_id: 'k-2025' },
  },
  {
    name: 'hetzner-fsn1',
    type: 's3',
    endpoint: 'http://127.0.0.1:9000',
    region: 'eu-central-1',
    force_path_style: false,
    access_key_id: 'AKIAEXAMPLE',
    allow_local: true,
  },
  {
    name: 'aws-dr',
    type: 's3',
    region: 'us-east-1',
    force_path_style: true,
    access_key_id: '${env:AWS_DR_KEY}',
    encryption: { mode: 'sse-kms', kms_key_id: 'arn:aws:kms:us-east-1:1:key/a', bucket_key_enabled: false },
  },
  {
    name: 'aws-s3',
    type: 's3',
    region: 'us-east-1',
    encryption: { mode: 'sse-s3', legacy_key_id: 'old' },
  },
  { name: 'plain', type: 'filesystem', path: '/srv/p', encryption: { mode: 'none', legacy_key_id: 'x' } },
];

test('an encryption edit keeps every field of every other backend (all variants)', () => {
  const before = JSON.parse(JSON.stringify(variants));
  for (const target of variants) {
    const patch = { mode: 'sse-s3' };
    const got = buildEncryptionSectionBody(target.name as string, patch, { backends: variants });
    const list = got.backends as Array<Record<string, unknown>>;
    assert.equal(list.length, variants.length);
    list.forEach((entry, i) => {
      if (entry.name === target.name) {
        assert.deepEqual(entry, { ...variants[i], encryption: { mode: 'sse-s3' } });
      } else {
        assert.deepEqual(entry, variants[i], `backend ${entry.name} changed while editing ${target.name}`);
      }
    });
  }
  assert.deepEqual(variants, before, 'the input must not be mutated');
});

test('the target keeps its own non-encryption fields (allow_local, endpoint)', () => {
  const got = buildEncryptionSectionBody('hetzner-fsn1', { mode: 'none' }, { backends: variants });
  const entry = (got.backends as Array<Record<string, unknown>>).find((b) => b.name === 'hetzner-fsn1');
  assert.equal(entry?.allow_local, true);
  assert.equal(entry?.endpoint, 'http://127.0.0.1:9000');
  assert.equal(entry?.force_path_style, false);
});

test('singleton (no backends list) uses backend_encryption', () => {
  const got = buildEncryptionSectionBody('default', { mode: 'aes256-gcm-proxy', key: 'k' }, {});
  assert.deepEqual(got, { backend_encryption: { mode: 'aes256-gcm-proxy', key: 'k' } });
});

test('a named backend that the section does not have is an error, not a list without it', () => {
  assert.throws(() => buildEncryptionSectionBody('gone', { mode: 'none' }, { backends: variants }), /Reload/);
});

test('encryption-block field-inclusion truth table', () => {
  const encOf = (patch: Parameters<typeof buildEncryptionSectionBody>[1]) =>
    buildEncryptionSectionBody('default', patch, {}).backend_encryption;
  assert.deepEqual(encOf({ mode: 'none' }), { mode: 'none' });
  assert.deepEqual(
    encOf({ mode: 'sse-kms', kms_key_id: 'arn:x', bucket_key_enabled: true }),
    { mode: 'sse-kms', kms_key_id: 'arn:x', bucket_key_enabled: true },
  );
  // null-clears MUST pass through (they mean "drop it" to the server).
  assert.deepEqual(
    encOf({ mode: 'none', legacy_key: null, legacy_key_id: null }),
    { mode: 'none', legacy_key: null, legacy_key_id: null },
  );
});

test('a proxy-AES key patch on a proxy-AES backend names the retired key id', () => {
  // Explore finding 2: Rotate key sent only the new key, and the server
  // dropped the old one. The UI cannot know the old key (GET redacts it),
  // but it knows its id: send it as legacy_key_id so the intent is
  // explicit, and clear key_id so the new key gets its own id.
  assert.deepEqual(aesKeyPatch('NEW', { mode: 'aes256-gcm-proxy', key_id: 'old-id' }), {
    mode: 'aes256-gcm-proxy',
    key: 'NEW',
    key_id: null,
    legacy_key_id: 'old-id',
  });
  // Enabling from another mode: nothing to retire.
  assert.deepEqual(aesKeyPatch('NEW', { mode: 'none' }), { mode: 'aes256-gcm-proxy', key: 'NEW' });
});
