import assert from 'node:assert/strict';
import { test } from 'vitest';
import { ADMIN_PATH_REMAP, isUnknownAdminPath, nearestAdminPath, resolveAdminPath } from '../adminPathRemap';

// The live IA's leaves — the membership test the app injects.
const KNOWN = new Set([
  'dashboard',
  'diagnostics/trace',
  'diagnostics/audit',
  'diagnostics/delta-efficiency',
  'access/credentials',
  'access/users',
  'access/groups',
  'access/external-auth',
  'access/admission',
  'storage/backends',
  'storage/buckets',
  'jobs',
  'integrations/event-delivery',
  'integrations/event-outbox',
  'system',
]);
const isKnown = (p: string) => KNOWN.has(p);

test('every remap target must be a live leaf', () => {
  // A dangling target would send the user to the dashboard fallback silently.
  for (const [from, to] of Object.entries(ADMIN_PATH_REMAP)) {
    assert.ok(KNOWN.has(to), `remap target for '${from}' is not a live leaf: '${to}'`);
  }
});

test('the full historical table (both old URL schemes)', () => {
  const CASES: Record<string, string> = {
    // flat aliases
    metrics: 'dashboard',
    users: 'access/users',
    groups: 'access/groups',
    auth: 'access/external-auth',
    backends: 'storage/backends',
    backend: 'storage/backends',
    compression: 'storage/backends',
    encryption: 'storage/backends',
    limits: 'system',
    security: 'system',
    logging: 'system',
    // 4-group scheme
    'diagnostics/dashboard': 'dashboard',
    'diagnostics/event-outbox': 'integrations/event-outbox',
    'configuration/admission': 'access/admission',
    'configuration/access': 'access/credentials',
    'configuration/access/credentials': 'access/credentials',
    'configuration/access/users': 'access/users',
    'configuration/access/groups': 'access/groups',
    'configuration/access/ext-auth': 'access/external-auth',
    'configuration/storage': 'storage/backends',
    'configuration/storage/backends': 'storage/backends',
    'configuration/storage/buckets': 'storage/buckets',
    'configuration/storage/encryption': 'storage/backends',
    'configuration/storage/replication': 'jobs',
    'configuration/storage/lifecycle': 'jobs',
    'configuration/recovery': 'system',
    'configuration/advanced': 'system',
    'configuration/advanced/listener': 'system',
    'configuration/advanced/caches': 'system',
    'configuration/advanced/limits': 'system',
    'configuration/advanced/logging': 'system',
    'configuration/advanced/sync': 'system',
    'configuration/advanced/event-delivery': 'integrations/event-delivery',
  };
  for (const [from, to] of Object.entries(CASES)) {
    assert.equal(resolveAdminPath(from, isKnown), to, `remap '${from}'`);
  }
});

test('passthroughs: live leaves resolve to themselves; setup is special', () => {
  for (const leaf of KNOWN) {
    assert.equal(resolveAdminPath(leaf, isKnown), leaf, `passthrough '${leaf}'`);
  }
  assert.equal(resolveAdminPath('setup', isKnown), 'setup');
});

test('slash trimming + empty + unknown → dashboard', () => {
  assert.equal(resolveAdminPath('/jobs/', isKnown), 'jobs');
  assert.equal(resolveAdminPath('', isKnown), 'dashboard');
  assert.equal(resolveAdminPath('totally/unknown', isKnown), 'dashboard');
});

// Browser-review item 20: an unknown admin deep link rendered the dashboard
// under the wrong URL, with no word. It now gets a not-found view that links
// the nearest real page.
test('unknown paths are detected; the nearest page shares the longest prefix', () => {
  assert.equal(isUnknownAdminPath('totally/unknown', isKnown), true);
  assert.equal(isUnknownAdminPath('', isKnown), false);
  assert.equal(isUnknownAdminPath('users', isKnown), false); // legacy remap
  assert.equal(isUnknownAdminPath('setup', isKnown), false);
  assert.equal(isUnknownAdminPath('access/users', isKnown), false);
  const leaves = [...KNOWN];
  assert.equal(nearestAdminPath('access/userz', leaves), 'access/users');
  assert.equal(nearestAdminPath('storage/nope', leaves), 'storage/backends');
  assert.equal(nearestAdminPath('jobs/42', leaves), 'jobs');
  assert.equal(nearestAdminPath('zzz', leaves), 'dashboard');
});
