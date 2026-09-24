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

// Fresh install: only the synthesized singleton, no buckets.
const fresh = describeExistingSetup([fs('default', './data', { is_synthesized: true })], 0, null);
assert.equal(fresh.configured, false);
assert.equal(fresh.kind, 'filesystem');

// Singleton with buckets: configured.
assert.equal(describeExistingSetup([fs('default', '/srv', { is_synthesized: true })], 2, null).configured, true);

// Named backends, S3 default (issue #92 item 20: the wizard preselected
// Filesystem on an S3-backed proxy).
const hunt = describeExistingSetup(
  [s3('hetzner-fsn1', 'http://127.0.0.1:29000', 'fsn1'), fs('local-disk', '/data/local')],
  5,
  'hetzner-fsn1',
);
assert.equal(hunt.configured, true);
assert.equal(hunt.kind, 's3');
assert.equal(hunt.s3Endpoint, 'http://127.0.0.1:29000');
assert.equal(hunt.s3Region, 'fsn1');
assert.equal(hunt.fsPath, '');
assert.equal(hunt.backendCount, 2);
assert.equal(hunt.bucketCount, 5);

// Default names a filesystem backend: its path carries over.
const local = describeExistingSetup([s3('a', 'http://x', 'r'), fs('b', '/srv/b')], 0, 'b');
assert.equal(local.kind, 'filesystem');
assert.equal(local.fsPath, '/srv/b');

// No backends at all.
const none = describeExistingSetup([], 0, null);
assert.equal(none.configured, false);
assert.equal(none.kind, null);

console.log('setup-detect regression tests passed');
