import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/setupDetect.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'setupDetect.ts',
});
const { describeExistingSetup, settingsAtRisk } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

const fs = (name, path, extra = {}) => ({ name, backend_type: 'filesystem', path, endpoint: null, region: null, force_path_style: null, ...extra });
const s3 = (name, endpoint, region, extra = {}) => ({ name, backend_type: 's3', path: null, endpoint, region, force_path_style: true, ...extra });

// ── settingsAtRisk: what applying the wizard (a WHOLE document) would lose ──
const fresh = { admission: {}, access: {}, storage: {}, advanced: {} };
assert.deepEqual(settingsAtRisk(fresh, []), [], 'a fresh install has nothing at risk');
// An env-set listen address is not in the file: it survives, so no warning.
assert.deepEqual(settingsAtRisk({ ...fresh, advanced: { listen_addr: '0.0.0.0:9000' } }, ['advanced.listen_addr']), []);
// …but a listen address written in the file would be dropped.
assert.deepEqual(settingsAtRisk({ ...fresh, advanced: { listen_addr: '0.0.0.0:9000' } }, []), ['the listen address']);

// Issue #92 round-2 review: a single legacy backend (no named backends, no
// bucket settings, no request rules) got no warning, and Apply dropped the
// rest of the file.
assert.deepEqual(
  settingsAtRisk({ ...fresh, storage: { s3: { endpoint: 'http://minio:9000', region: 'us-east-1' } } }, []),
  ['the storage backend settings'],
);
assert.deepEqual(settingsAtRisk({ ...fresh, storage: { filesystem: '/srv/dgp' } }, []), ['the storage backend settings']);
// …unless the environment sets that backend.
assert.deepEqual(settingsAtRisk({ ...fresh, storage: { backend: { type: 's3' } } }, ['storage.backend']), []);
assert.deepEqual(settingsAtRisk({ ...fresh, access: { iam_mode: 'declarative' } }, []), ['the declarative IAM mode']);
assert.deepEqual(settingsAtRisk({ ...fresh, access: { access_key_id: 'ak' } }, []), ['access settings']);
assert.deepEqual(settingsAtRisk({ ...fresh, advanced: { config_sync_bucket: 'dgp-sync' } }, []), ['the configuration sync bucket']);

// The hunt proxy: everything named, in a stable order, deduplicated.
const hunt = {
  admission: { blocks: [{ name: 'block-bad-actor', match: {}, action: 'deny' }] },
  access: {},
  storage: {
    backends: [{ name: 'hetzner-fsn1', type: 's3' }],
    default_backend: 'hetzner-fsn1',
    buckets: { releases: { backend: 'hetzner-fsn1' }, downloads: { public_prefixes: ['public/'] } },
    replication: { enabled: true, rules: [{ name: 'r' }] },
    lifecycle: { rules: [{ name: 'l' }] },
  },
  advanced: { listen_addr: '1.2.3.4:19080', log_level: 'warn', event_delivery: { enabled: true } },
};
assert.deepEqual(settingsAtRisk(hunt, []), [
  'request rules',
  'bucket settings',
  'replication rules',
  'lifecycle rules',
  'storage backends',
  'event delivery',
  'the listen address',
  'advanced settings',
]);
// Empty lists and nulls are defaults, not settings.
assert.deepEqual(settingsAtRisk({ ...fresh, admission: { blocks: [] }, storage: { default_backend: null } }, []), []);

// ── describeExistingSetup: configured = anything at risk; seeding ──
assert.equal(describeExistingSetup([fs('default', './data', { is_synthesized: true })], [], null).configured, false);
assert.equal(describeExistingSetup([fs('default', './data', { is_synthesized: true })], ['access settings'], null).configured, true);

// Named backends, S3 default (issue #92 item 20: the wizard preselected
// Filesystem on an S3-backed proxy).
const h = describeExistingSetup(
  [s3('hetzner-fsn1', 'http://127.0.0.1:29000', 'fsn1'), fs('local-disk', '/data/local')],
  ['storage backends'],
  'hetzner-fsn1',
);
assert.equal(h.configured, true);
assert.equal(h.kind, 's3');
assert.equal(h.s3Endpoint, 'http://127.0.0.1:29000');
assert.equal(h.s3Region, 'fsn1');
assert.equal(h.fsPath, '');
assert.deepEqual(h.atRisk, ['storage backends']);

// Default names a filesystem backend: its path carries over.
const local = describeExistingSetup([s3('a', 'http://x', 'r'), fs('b', '/srv/b')], [], 'b');
assert.equal(local.kind, 'filesystem');
assert.equal(local.fsPath, '/srv/b');

// No backends at all.
const none = describeExistingSetup([], [], null);
assert.equal(none.configured, false);
assert.equal(none.kind, null);

console.log('setup-detect regression tests passed');
