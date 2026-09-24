import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Regression guard for issue #92 item 11: the browser's backend chips were
// guessed by a regex over backend names ("HZ" in red, "LOC" in teal, "S3"
// when nothing matched, even when no origin data was loaded). The chip now
// shows the real backend name, only when the data is there, and only when the
// buckets span more than one backend.

const source = await readFile(new URL('../src/bucketBackend.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'bucketBackend.ts',
});
const { backendChipLabel, showBackendChips, describeBackend } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

// No data (non-admin session, origins not loaded) → no chip, never a guess.
assert.equal(backendChipLabel(undefined), null);
assert.equal(backendChipLabel({}), null);
assert.equal(backendChipLabel({ backendType: 's3', backendEndpoint: 'https://fsn1.your-objectstorage.com' }), null);
assert.equal(backendChipLabel({ backendName: '  ' }), null);

// The label is the real name, verbatim — no provider inference.
assert.equal(backendChipLabel({ backendName: 'hetzner-fsn1', backendType: 's3' }), 'hetzner-fsn1');
assert.equal(backendChipLabel({ backendName: 'local-disk', backendType: 'filesystem' }), 'local-disk');

// Chips only when there is more than one backend to tell apart.
assert.equal(showBackendChips([]), false);
assert.equal(showBackendChips([undefined, undefined]), false);
assert.equal(showBackendChips([{ backendName: 'default' }, { backendName: 'default' }]), false);
assert.equal(showBackendChips([{ backendName: 'hetzner-fsn1' }, { backendName: 'local-disk' }]), true);
assert.equal(showBackendChips([{ backendName: 'hetzner-fsn1' }, undefined]), false);

assert.equal(describeBackend(undefined), '');
assert.equal(
  describeBackend({ backendName: 'local-disk', backendType: 'filesystem', backendPath: '/srv/dgp' }),
  'Backend: local-disk\nType: filesystem\nPath: /srv/dgp',
);

console.log('bucket-backend regression checks passed');
