import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Issue #92 item 16: the account menu did not say who is signed in or which
// auth mode is active.

const source = await readFile(new URL('../src/identitySummary.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'identitySummary.ts',
});
const { identitySummary } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);

assert.deepEqual(identitySummary(null, 'AKIA1'), { name: 'AKIA1', detail: 'Reading your account…' });
assert.match(identitySummary({ mode: 'open', user: null }, undefined).detail, /Authentication is off/);
assert.equal(identitySummary({ mode: 'open', user: null }, undefined).name, 'Open access');
assert.match(identitySummary({ mode: 'bootstrap', user: null }, 'k').detail, /^Bootstrap mode/);
assert.equal(identitySummary({ mode: 'bootstrap', user: null }, 'k').name, 'Administrator');

const admin = { mode: 'iam', user: { name: 'dana', access_key_id: 'dana-admin-key', is_admin: true } };
assert.deepEqual(identitySummary(admin, undefined), { name: 'dana', detail: 'Administrator (IAM) · key dana-admin-key' });
const user = { mode: 'iam', user: { name: 'ci-uploader', access_key_id: 'CI1', is_admin: false } };
assert.deepEqual(identitySummary(user, undefined), { name: 'ci-uploader', detail: 'User (IAM) · key CI1' });
// IAM mode before the user resolved: fall back to the access key.
assert.deepEqual(identitySummary({ mode: 'iam', user: null }, 'AK2'), { name: 'AK2', detail: 'User (IAM) · key AK2' });

console.log('identity-summary regression checks passed');
