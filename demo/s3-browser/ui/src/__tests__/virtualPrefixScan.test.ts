/** src/permissions.ts — virtual prefix scan suppression. */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { virtualWritableChildren, canRequestPrefixUsageScan, isVirtualFolderPrefix } from '../permissions';

test('virtual prefix scan suppression checks', () => {
  const realFolders = ['team-a/'];
  const writablePrefixes = ['team-a/', 'team-b/sub/', 'team-c/releases/canary/'];
  const virtualFolders = virtualWritableChildren('', realFolders, writablePrefixes);

  assert.deepEqual(virtualFolders, ['team-b/', 'team-c/']);
  assert.equal(isVirtualFolderPrefix('team-a/', virtualFolders), false);
  assert.equal(isVirtualFolderPrefix('team-b/', virtualFolders), true);
  assert.equal(canRequestPrefixUsageScan('team-a/', virtualFolders, true), true);
  assert.equal(canRequestPrefixUsageScan('team-b/', virtualFolders, true), false);
  assert.equal(canRequestPrefixUsageScan('team-c/', virtualFolders, true), false);
  assert.equal(canRequestPrefixUsageScan('team-c/releases/', virtualFolders, true), true);
  assert.equal(canRequestPrefixUsageScan('team-a/', virtualFolders, false), false);
  assert.equal(canRequestPrefixUsageScan('team-c/releases/', virtualFolders, false), false);
});
