/** src/backendEncryptionPayload.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { buildEncryptionSectionBody } from '../backendEncryptionPayload';

/** Mirror of the (unexported) `EncryptionPatch` in backendEncryptionPayload.ts. */
interface EncryptionPatch {
  mode: string;
  key?: string;
  key_id?: string;
  kms_key_id?: string;
  bucket_key_enabled?: boolean;
  legacy_key?: string | null;
  legacy_key_id?: string | null;
}

/** Mirror of the (unexported) `BackendShapeSource` in backendEncryptionPayload.ts. */
interface BackendShapeSource {
  name: string;
  backend_type: string;
  path?: string | null;
  endpoint?: string | null;
  region?: string | null;
  force_path_style?: boolean | null;
}

// ── Oracle: the EXACT inline logic BackendsPanel.handleEncryptionApply
//    used before the extraction. The builder must match this byte-for-byte
//    (compared via JSON.stringify) so the admin-API wire contract is
//    unchanged for identical user input. ────────────────────────────────
function oracleEncBody(patch: EncryptionPatch): Record<string, unknown> {
  const encBody: Record<string, unknown> = { mode: patch.mode };
  if (patch.key !== undefined) encBody.key = patch.key;
  if (patch.key_id !== undefined) encBody.key_id = patch.key_id;
  if (patch.kms_key_id !== undefined) encBody.kms_key_id = patch.kms_key_id;
  if (patch.bucket_key_enabled !== undefined) encBody.bucket_key_enabled = patch.bucket_key_enabled;
  if (patch.legacy_key !== undefined) encBody.legacy_key = patch.legacy_key;
  if (patch.legacy_key_id !== undefined) encBody.legacy_key_id = patch.legacy_key_id;
  return encBody;
}

function oracleBody(
  backendName: string,
  patch: EncryptionPatch,
  backends: BackendShapeSource[],
): Record<string, unknown> {
  const encBody = oracleEncBody(patch);
  let body: Record<string, unknown>;
  if (backendName === 'default' && backends.length === 1 && backends[0].name === 'default') {
    body = { backend_encryption: encBody };
  } else {
    const list = backends.map((b) => {
      const backendShape: Record<string, unknown> = { name: b.name, type: b.backend_type };
      if (b.path) backendShape.path = b.path;
      if (b.endpoint) backendShape.endpoint = b.endpoint;
      if (b.region) backendShape.region = b.region;
      if (b.force_path_style !== null) backendShape.force_path_style = b.force_path_style;
      if (b.name === backendName) backendShape.encryption = encBody;
      return backendShape;
    });
    body = { backends: list };
  }
  return body;
}

function eq(a: unknown, b: unknown, msg?: string): void {
  // assert.equal's `message` overloads don't accept `string | undefined`
  // directly, so branch on presence instead of forwarding the optional arg.
  if (msg !== undefined) assert.equal(JSON.stringify(a), JSON.stringify(b), msg);
  else assert.equal(JSON.stringify(a), JSON.stringify(b));
}

// Backend fixtures matching the live BackendInfo shape (force_path_style
// is boolean|null, never undefined).
const fsDefault: BackendShapeSource = {
  name: 'default', backend_type: 'filesystem',
  path: './data', endpoint: null, region: null, force_path_style: null,
};
const s3Hetzner: BackendShapeSource = {
  name: 'hetzner', backend_type: 's3',
  path: null, endpoint: 'https://fsn1.example.com', region: 'eu-central-1', force_path_style: true,
};
const fsLocal: BackendShapeSource = {
  name: 'local', backend_type: 'filesystem',
  path: '/srv/data', endpoint: null, region: null, force_path_style: null,
};

test('encryption-block field-inclusion truth table (singleton path)', () => {
  const encOf = (patch: EncryptionPatch) =>
    buildEncryptionSectionBody('default', patch, [fsDefault]).backend_encryption;

  eq(encOf({ mode: 'none' }), { mode: 'none' }, 'none → mode only');
  eq(
    encOf({ mode: 'aes256-gcm-proxy', key: 'abc' }),
    { mode: 'aes256-gcm-proxy', key: 'abc' },
    'proxy-AES → mode + key',
  );
  eq(
    encOf({ mode: 'sse-kms', kms_key_id: 'arn:x', bucket_key_enabled: true }),
    { mode: 'sse-kms', kms_key_id: 'arn:x', bucket_key_enabled: true },
    'sse-kms → mode + kms_key_id + bucket_key_enabled',
  );
  // legacy_key null-clear MUST pass through (it's `!== undefined`).
  eq(
    encOf({ mode: 'none', legacy_key: null, legacy_key_id: null }),
    { mode: 'none', legacy_key: null, legacy_key_id: null },
    'legacy_key null-clear passes through',
  );
});

test('singleton path -> { backend_encryption: <encBody> }', () => {
  const patch: EncryptionPatch = { mode: 'aes256-gcm-proxy', key: 'deadbeef' };
  const got = buildEncryptionSectionBody('default', patch, [fsDefault]);
  eq(got, oracleBody('default', patch, [fsDefault]), 'singleton matches oracle');
  assert.ok('backend_encryption' in got, 'singleton uses backend_encryption key');
  assert.ok(!('backends' in got), 'singleton must NOT emit backends array');
});

test('a backend NAMED "default" but NOT a lone singleton takes the LIST path', () => {
  const patch: EncryptionPatch = { mode: 'sse-s3' };
  const backends = [fsDefault, s3Hetzner];
  const got = buildEncryptionSectionBody('default', patch, backends);
  eq(got, oracleBody('default', patch, backends), 'named-default-in-list matches oracle');
  assert.ok('backends' in got, 'two-entry list → backends array even when target is named default');
});

test('named-list path: only the target entry gets `encryption`', () => {
  const patch: EncryptionPatch = { mode: 'sse-kms', kms_key_id: 'arn:aws:kms:...', bucket_key_enabled: false };
  const backends = [fsLocal, s3Hetzner];
  const got = buildEncryptionSectionBody('hetzner', patch, backends);
  eq(got, oracleBody('hetzner', patch, backends), 'named-list matches oracle');
  // Structural assertions on the wire shape.
  const list = got.backends as Record<string, unknown>[];
  assert.equal(list.length, 2);
  // local: filesystem, no endpoint/region, force_path_style null → omitted.
  eq(list[0], { name: 'local', type: 'filesystem', path: '/srv/data' }, 'fs sibling shape');
  assert.ok(!('encryption' in list[0]), 'non-target sibling has no encryption block');
  // hetzner: target → carries encryption; force_path_style true → kept.
  assert.equal(list[1].name, 'hetzner');
  assert.equal(list[1].type, 's3');
  assert.equal(list[1].endpoint, 'https://fsn1.example.com');
  assert.equal(list[1].region, 'eu-central-1');
  assert.equal(list[1].force_path_style, true);
  eq(list[1].encryption, oracleEncBody(patch), 'target carries the encryption body');
});

test('force_path_style === false is a real value → must be KEPT', () => {
  // (the original used `!== null`, so false is emitted, not dropped).
  const s3PathFalse: BackendShapeSource = { ...s3Hetzner, name: 's3b', force_path_style: false };
  const patch: EncryptionPatch = { mode: 'none' };
  const backends = [s3PathFalse];
  const got = buildEncryptionSectionBody('s3b', patch, backends);
  eq(got, oracleBody('s3b', patch, backends), 'force_path_style false matches oracle');
  const list = got.backends as Record<string, unknown>[];
  assert.equal(list[0].force_path_style, false, 'force_path_style:false is preserved');
});
