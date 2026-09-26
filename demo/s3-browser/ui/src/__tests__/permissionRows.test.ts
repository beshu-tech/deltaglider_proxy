import assert from 'node:assert/strict';
import { test } from 'vitest';
import { permissionsToRows, rowsToPermissions, freshPermissionRowId, type PermissionRow } from '../components/permissionRows';
import type { IamPermission } from '../adminApi';

test('freshPermissionRowId: stable, unique', () => {
  const a = freshPermissionRowId();
  const b = freshPermissionRowId();
  assert.notEqual(a, b);
  assert.match(a, /^perm-\d+$/);
});

test('permissionsToRows assigns a stable _uiId to every row', () => {
  const perms: IamPermission[] = [
    { id: 1, effect: 'Allow', actions: ['read', 'list'], resources: ['bucket/*'] },
    { id: 2, effect: 'Deny', actions: ['delete'], resources: ['bucket/secret/*'], conditions: { IpAddress: { 'aws:SourceIp': ['10.0.0.0/8'] } } },
  ];
  const rows = permissionsToRows(perms);
  assert.equal(rows.length, 2);
  assert.ok(rows[0]._uiId, 'row 0 must have a _uiId');
  assert.ok(rows[1]._uiId, 'row 1 must have a _uiId');
  assert.notEqual(rows[0]._uiId, rows[1]._uiId, 'ids must be unique per row');
  // fields carried through — resources stay an ARRAY (never comma-joined)
  assert.deepEqual(rows[0].resources, ['bucket/*']);
  assert.deepEqual(rows[1].actions, ['delete']);
  assert.deepEqual(rows[1].conditions, { IpAddress: { 'aws:SourceIp': ['10.0.0.0/8'] } });
});

test('THE REGRESSION: _uiId NEVER leaks into the wire IamPermission', () => {
  const perms: IamPermission[] = [
    { id: 1, effect: 'Allow', actions: ['read', 'list'], resources: ['bucket/*'] },
    { id: 2, effect: 'Deny', actions: ['delete'], resources: ['bucket/secret/*'], conditions: { IpAddress: { 'aws:SourceIp': ['10.0.0.0/8'] } } },
  ];
  const rows = permissionsToRows(perms);
  const wire = rowsToPermissions(rows);
  assert.equal(wire.length, 2);
  for (const p of wire) {
    assert.ok(!('_uiId' in p), '_uiId must be stripped from the wire permission');
  }
  // wire content is correct (resources normalized + split)
  assert.deepEqual(wire[0].resources, ['bucket/*']);
  assert.equal(wire[0].effect, 'Allow');
  assert.deepEqual(wire[1].actions, ['delete']);
  assert.deepEqual(wire[1].conditions, { IpAddress: { 'aws:SourceIp': ['10.0.0.0/8'] } });
});

test('rows with a manually-attached _uiId also strip cleanly', () => {
  const manual = rowsToPermissions([
    { _uiId: 'perm-999', effect: 'Allow', actions: ['read'], resources: ['b/*'] },
  ]);
  assert.equal(manual.length, 1);
  assert.ok(!('_uiId' in manual[0]));
  assert.deepEqual(manual[0].resources, ['b/*']);
});

test('empty/incomplete rows are dropped (unchanged contract)', () => {
  const dropped = rowsToPermissions([
    { _uiId: 'perm-1', effect: 'Allow', actions: [], resources: ['b/*'] }, // no actions
    { _uiId: 'perm-2', effect: 'Allow', actions: ['read'], resources: ['  '] }, // blank resources
    { _uiId: 'perm-3', effect: 'Allow', actions: ['read'], resources: ['b/*'] }, // keep
  ]);
  assert.equal(dropped.length, 1);
  assert.deepEqual(dropped[0].resources, ['b/*']);
});

test('THE data-loss regression: a comma-bearing pattern is preserved', () => {
  // The old comma-join/split flattened resources; a pattern with a literal comma
  // (valid in an S3 key) must round-trip as ONE entry, not be split into two.
  const commaRows: PermissionRow[] = permissionsToRows([
    { id: 3, effect: 'Allow', actions: ['read'], resources: ['releases/a,b/*', 'db/*'] },
  ]);
  assert.deepEqual(commaRows[0].resources, ['releases/a,b/*', 'db/*'], 'comma stays intact on load');
  const commaWire = rowsToPermissions(commaRows);
  assert.deepEqual(commaWire[0].resources, ['releases/a,b/*', 'db/*'], 'comma stays ONE entry through save');
});
