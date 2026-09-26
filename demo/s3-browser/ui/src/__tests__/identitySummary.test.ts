// Issue #92 item 16: the account menu did not say who is signed in or which
// auth mode is active.
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { identitySummary } from '../identitySummary';

test('identitySummary: pre-resolution and open/bootstrap modes', () => {
  assert.deepEqual(identitySummary(null, 'AKIA1'), { name: 'AKIA1', detail: 'Reading your account…' });
  assert.match(identitySummary({ mode: 'open', user: null }, undefined).detail, /Authentication is off/);
  assert.equal(identitySummary({ mode: 'open', user: null }, undefined).name, 'Open access');
  assert.match(identitySummary({ mode: 'bootstrap', user: null }, 'k').detail, /^Bootstrap mode/);
  assert.equal(identitySummary({ mode: 'bootstrap', user: null }, 'k').name, 'Administrator');
});

test('identitySummary: IAM mode admin/user, and pre-resolution fallback', () => {
  const admin = { mode: 'iam' as const, user: { name: 'dana', access_key_id: 'dana-admin-key', is_admin: true } };
  assert.deepEqual(identitySummary(admin, undefined), { name: 'dana', detail: 'Administrator (IAM) · key dana-admin-key' });
  const user = { mode: 'iam' as const, user: { name: 'ci-uploader', access_key_id: 'CI1', is_admin: false } };
  assert.deepEqual(identitySummary(user, undefined), { name: 'ci-uploader', detail: 'User (IAM) · key CI1' });
  // IAM mode before the user resolved: fall back to the access key.
  assert.deepEqual(identitySummary({ mode: 'iam', user: null }, 'AK2'), { name: 'AK2', detail: 'User (IAM) · key AK2' });
});

test('identitySummary: bootstrap password used while in IAM mode', () => {
  // Bootstrap password in IAM mode: whoami reports a synthetic 'admin' user
  // with access key 'bootstrap'. It is not an IAM user and has no key.
  const bootstrapInIam = {
    mode: 'iam' as const,
    auth_method: 'bootstrap' as const,
    user: { name: 'admin', access_key_id: 'bootstrap', is_admin: true },
  };
  assert.deepEqual(identitySummary(bootstrapInIam, undefined), {
    name: 'Administrator',
    detail: 'Signed in with the bootstrap password, not as an IAM user.',
  });

  // A real IAM user whose access key is literally 'bootstrap' is an IAM user.
  const iamNamedBootstrap = {
    mode: 'iam' as const,
    auth_method: 'iam' as const,
    user: { name: 'ops', access_key_id: 'bootstrap', is_admin: false },
  };
  assert.deepEqual(identitySummary(iamNamedBootstrap, undefined), { name: 'ops', detail: 'User (IAM) · key bootstrap' });
});
