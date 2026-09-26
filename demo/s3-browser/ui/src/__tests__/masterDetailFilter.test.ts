/** src/masterDetailFilter.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { userPermissionSummary, groupPermissionSummary, filterItems } from '../masterDetailFilter';

interface PermissionRule {
  actions: string[];
  resources: string[];
}

const rule = (actions: string[], resources: string[]): PermissionRule => ({ actions, resources });

test('userPermissionSummary (full truth table)', () => {
  // SSO user, no direct rules -> null (badge + detail panel convey context).
  assert.equal(userPermissionSummary({ permissions: [], auth_source: 'external' }), null);
  assert.equal(userPermissionSummary({ permissions: [], auth_source: 'external', group_ids: [1, 2] }), null);
  // Local user, no rules, no groups -> No access.
  assert.equal(userPermissionSummary({ permissions: [] }), 'No access');
  assert.equal(userPermissionSummary({ permissions: [], auth_source: 'local' }), 'No access');
  // Local user, no direct rules but groups -> inheritance label (Wave 11 UX-5).
  assert.equal(userPermissionSummary({ permissions: [], group_ids: [1] }), '1 group (inherited)');
  assert.equal(userPermissionSummary({ permissions: [], group_ids: [1, 2, 3] }), '3 groups (inherited)');
  // Full admin (wildcard actions AND resources).
  assert.equal(userPermissionSummary({ permissions: [rule(['*'], ['*'])] }), 'Full admin');
  assert.equal(userPermissionSummary({ permissions: [rule(['read'], ['b/*']), rule(['*'], ['*'])] }), 'Full admin');
  // Wildcard action only / resource only is NOT full admin.
  assert.equal(userPermissionSummary({ permissions: [rule(['*'], ['b/*'])] }), '1 rule');
  assert.equal(userPermissionSummary({ permissions: [rule(['read'], ['*'])] }), '1 rule');
  // Rule counting + pluralization.
  assert.equal(userPermissionSummary({ permissions: [rule(['read'], ['b/*'])] }), '1 rule');
  assert.equal(userPermissionSummary({ permissions: [rule(['read'], ['a']), rule(['write'], ['b'])] }), '2 rules');
  // Rules + groups composite label.
  assert.equal(userPermissionSummary({ permissions: [rule(['read'], ['a'])], group_ids: [1] }), '1 rule · 1 group');
  assert.equal(
    userPermissionSummary({ permissions: [rule(['read'], ['a']), rule(['write'], ['b'])], group_ids: [1, 2] }),
    '2 rules · 2 groups',
  );
  // Full admin short-circuits before the groups composite.
  assert.equal(userPermissionSummary({ permissions: [rule(['*'], ['*'])], group_ids: [1, 2] }), 'Full admin');
});

test('groupPermissionSummary', () => {
  assert.equal(groupPermissionSummary({ permissions: [] }), 'No permissions');
  assert.equal(groupPermissionSummary({ permissions: [rule(['*'], ['*'])] }), 'Full access');
  assert.equal(groupPermissionSummary({ permissions: [rule(['*'], ['b/*'])] }), '1 rule');
  assert.equal(groupPermissionSummary({ permissions: [rule(['read'], ['a'])] }), '1 rule');
  assert.equal(groupPermissionSummary({ permissions: [rule(['read'], ['a']), rule(['write'], ['b'])] }), '2 rules');
});

test('filterItems (case-insensitive, identity on empty query)', () => {
  const users = [
    { name: 'Alice', access_key_id: 'AKIA111' },
    { name: 'Bob', access_key_id: 'AKIA222' },
    { name: 'carol', access_key_id: 'ZZZ333' },
  ];
  const byUserFields = (u: (typeof users)[number]) => [u.name, u.access_key_id];
  // Empty query returns the SAME array reference (identity) — matches inline `: items`.
  assert.equal(filterItems(users, '', byUserFields), users);
  // Match on name (case-insensitive).
  assert.deepEqual(filterItems(users, 'ali', byUserFields).map((u) => u.name), ['Alice']);
  assert.deepEqual(filterItems(users, 'BOB', byUserFields).map((u) => u.name), ['Bob']);
  // Match on access_key_id.
  assert.deepEqual(filterItems(users, 'akia2', byUserFields).map((u) => u.name), ['Bob']);
  assert.deepEqual(filterItems(users, 'akia', byUserFields).map((u) => u.name), ['Alice', 'Bob']);
  // No match -> empty.
  assert.deepEqual(filterItems(users, 'nope', byUserFields), []);
  // Null/undefined fields are tolerated (treated as empty string).
  const withNull: { name: string | null; access_key_id: string | null }[] = [
    { name: 'x', access_key_id: null },
    { name: null, access_key_id: 'KEY' },
  ];
  assert.deepEqual(filterItems(withNull, 'key', (i) => [i.name, i.access_key_id]).length, 1);

  // Group filter only matches name.
  const groups = [{ name: 'developers' }, { name: 'admins' }];
  const byGroupFields = (g: (typeof groups)[number]) => [g.name];
  assert.equal(filterItems(groups, '', byGroupFields), groups);
  assert.deepEqual(filterItems(groups, 'DEV', byGroupFields).map((g) => g.name), ['developers']);
  assert.deepEqual(filterItems(groups, 'x', byGroupFields), []);
});
