/** src/backendEncryptionPayload.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  aesKeyPatch,
  buildClearLegacySectionBody,
  buildEncryptionSectionBody,
  buildRefusePlaintextSectionBody,
  encryptionKeyEnvVar,
} from '../backendEncryptionPayload';

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
  // Enabling from another mode: nothing to retire, and the objects already
  // there are plaintext: keep serving them until the re-encrypt job (#120).
  assert.deepEqual(aesKeyPatch('NEW', { mode: 'none' }), {
    mode: 'aes256-gcm-proxy',
    key: 'NEW',
    plaintext_reads: 'serve',
  });
});

test('clearing the legacy key on the singleton nulls only the legacy fields', () => {
  assert.deepEqual(buildClearLegacySectionBody('default', {}), {
    backend_encryption: { legacy_key: null, legacy_key_id: null },
  });
});

test('clearing the legacy key on a named backend keeps its key_id and every sibling', () => {
  const body = buildClearLegacySectionBody('local-enc', { backends: variants }) as {
    backends: Array<Record<string, unknown>>;
  };
  assert.equal(body.backends.length, variants.length);
  const target = body.backends.find((b) => b.name === 'local-enc');
  assert.deepEqual(target, {
    name: 'local-enc',
    type: 'filesystem',
    path: '/srv/enc',
    // The explicit key_id stays: dropping it would change the id new objects carry.
    encryption: { mode: 'aes256-gcm-proxy', key_id: 'k-2026', legacy_key: null, legacy_key_id: null },
  });
  for (const v of variants.filter((b) => b.name !== 'local-enc')) {
    assert.deepEqual(body.backends.find((b) => b.name === v.name), v);
  }
  assert.throws(() => buildClearLegacySectionBody('gone', { backends: variants }), /Reload the page/);
});

// Mirrors backend_encryption_env_names (src/config/mod.rs): "default" uses the
// short name, others upper-case the name with '-' and '.' mapped to '_'.
test('encryptionKeyEnvVar matches the server env-var names', () => {
  assert.equal(encryptionKeyEnvVar('default'), 'DGP_ENCRYPTION_KEY');
  assert.equal(encryptionKeyEnvVar('hetzner-fsn1'), 'DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY');
  assert.equal(encryptionKeyEnvVar('aws.dr'), 'DGP_BACKEND_AWS_DR_ENCRYPTION_KEY');
});

test('a key change keeps the backend plaintext_reads (#120)', () => {
  const storage = {
    backends: [
      {
        name: 'local-disk',
        encryption: { mode: 'aes256-gcm-proxy', key_id: 'old', plaintext_reads: 'serve' },
      },
    ],
  };
  const body = buildEncryptionSectionBody('local-disk', aesKeyPatch('a'.repeat(64), { mode: 'aes256-gcm-proxy', key_id: 'old' }), storage) as {
    backends: Array<{ encryption: Record<string, unknown> }>;
  };
  assert.equal(body.backends[0].encryption.plaintext_reads, 'serve');
  const off = buildEncryptionSectionBody('local-disk', { mode: 'none' }, storage) as {
    backends: Array<{ encryption: Record<string, unknown> }>;
  };
  assert.equal(off.backends[0].encryption.plaintext_reads, undefined, 'mode none has no such field');
});

test('refusing plaintext changes only plaintext_reads of a backend (#120)', () => {
  assert.deepEqual(buildRefusePlaintextSectionBody('default', {}), {
    backend_encryption: { plaintext_reads: 'refuse' },
  });
  const storage = {
    backends: [{ name: 'local-disk', encryption: { mode: 'aes256-gcm-proxy', key_id: 'k', plaintext_reads: 'serve' } }],
  };
  assert.deepEqual(buildRefusePlaintextSectionBody('local-disk', storage), {
    backends: [{ name: 'local-disk', encryption: { mode: 'aes256-gcm-proxy', key_id: 'k', plaintext_reads: 'refuse' } }],
  });
});

// The three builders share one target rule (`withBackendEncryption`):
// singleton → `backend_encryption`; named → the section's own list with only
// the target's block changed; a missing name → an error; no input mutation.
type Build = (name: string, storage: { backends?: Array<Record<string, unknown>> }) => Record<string, unknown>;
const builders: Array<[string, Build]> = [
  ['encryption', (n, s) => buildEncryptionSectionBody(n, { mode: 'sse-s3' }, s)],
  ['clear legacy', (n, s) => buildClearLegacySectionBody(n, s)],
  ['refuse plaintext', (n, s) => buildRefusePlaintextSectionBody(n, s)],
];

test('every builder follows the same target rule', () => {
  for (const [label, build] of builders) {
    assert.deepEqual(Object.keys(build('default', {})), ['backend_encryption'], label);
    assert.throws(() => build('gone', { backends: variants }), /Reload the page/, label);
    // A non-default name with no list is a stale page, not a singleton patch.
    assert.throws(() => build('local-enc', {}), /Reload the page/, label);
    const before = JSON.parse(JSON.stringify(variants));
    const list = build('local-enc', { backends: variants }).backends as Array<Record<string, unknown>>;
    assert.equal(list.length, variants.length, label);
    list.forEach((entry, i) => {
      const rest = { ...entry, encryption: undefined };
      const wasRest = { ...variants[i], encryption: undefined };
      assert.deepEqual(rest, wasRest, `${label}: non-encryption fields of ${String(entry.name)}`);
      if (entry.name !== 'local-enc') assert.deepEqual(entry, variants[i], `${label}: sibling ${String(entry.name)}`);
    });
    assert.deepEqual(variants, before, `${label}: the input must not be mutated`);
  }
});
