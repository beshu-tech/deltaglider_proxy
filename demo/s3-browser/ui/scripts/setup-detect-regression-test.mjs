import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/setupDetect.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'setupDetect.ts',
});
const { describeExistingSetup } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

const fs = (name, path, extra = {}) => ({ name, backend_type: 'filesystem', path, endpoint: null, region: null, force_path_style: null, ...extra });
const s3 = (name, endpoint, region, extra = {}) => ({ name, backend_type: 's3', path: null, endpoint, region, force_path_style: true, ...extra });

const none0 = { bucketSettings: 0, requestRules: 0 };

// Fresh install: only the synthesized singleton, nothing configured.
const fresh = describeExistingSetup([fs('default', './data', { is_synthesized: true })], none0, null);
assert.equal(fresh.configured, false);
assert.equal(fresh.kind, 'filesystem');
assert.equal(fresh.backendCount, 0, 'the synthesized singleton is not a named backend');

// Issue #92 review: a fresh proxy on a MinIO that already HAS buckets is not
// "configured" — the buckets live on the storage, the wizard does not touch
// them. Only the proxy's own configuration counts; the signature no longer
// takes a bucket count at all.
const freshS3 = describeExistingSetup(
  [s3('default', 'http://minio:9000', 'us-east-1', { is_synthesized: true })],
  none0,
  null,
);
assert.equal(freshS3.configured, false);
assert.equal(freshS3.kind, 's3');

// Singleton with bucket settings, or with request rules: configured.
assert.equal(describeExistingSetup([fs('default', '/srv', { is_synthesized: true })], { bucketSettings: 2, requestRules: 0 }, null).configured, true);
assert.equal(describeExistingSetup([fs('default', '/srv', { is_synthesized: true })], { bucketSettings: 0, requestRules: 1 }, null).configured, true);

// Named backends, S3 default (issue #92 item 20: the wizard preselected
// Filesystem on an S3-backed proxy).
const hunt = describeExistingSetup(
  [s3('hetzner-fsn1', 'http://127.0.0.1:29000', 'fsn1'), fs('local-disk', '/data/local')],
  { bucketSettings: 5, requestRules: 1 },
  'hetzner-fsn1',
);
assert.equal(hunt.configured, true);
assert.equal(hunt.kind, 's3');
assert.equal(hunt.s3Endpoint, 'http://127.0.0.1:29000');
assert.equal(hunt.s3Region, 'fsn1');
assert.equal(hunt.fsPath, '');
assert.equal(hunt.backendCount, 2);
assert.equal(hunt.bucketSettingsCount, 5);
assert.equal(hunt.requestRuleCount, 1);

// Default names a filesystem backend: its path carries over.
const local = describeExistingSetup([s3('a', 'http://x', 'r'), fs('b', '/srv/b')], none0, 'b');
assert.equal(local.kind, 'filesystem');
assert.equal(local.fsPath, '/srv/b');

// No backends at all.
const none = describeExistingSetup([], none0, null);
assert.equal(none.configured, false);
assert.equal(none.kind, null);

console.log('setup-detect regression tests passed');
