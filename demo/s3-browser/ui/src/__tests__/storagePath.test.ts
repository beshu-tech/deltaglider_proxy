/** src/storagePath.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { formatResourcePattern, normalizePrefix, normalizeResourcePattern, parseResourcePattern } from '../storagePath';

test('normalizePrefix', () => {
  assert.equal(normalizePrefix(' /team//${iam:username}/builds '), 'team/${iam:username}/builds/');
  assert.equal(normalizePrefix(''), '');
  assert.equal(normalizePrefix('///'), '');
});

test('parseResourcePattern', () => {
  assert.deepEqual(parseResourcePattern('*'), {
    bucket: '',
    prefix: '',
    wildcard: true,
    global: true,
  });
  assert.deepEqual(parseResourcePattern('artifacts/team-a/*'), {
    bucket: 'artifacts',
    prefix: 'team-a/',
    wildcard: true,
    global: false,
  });
});

test('formatResourcePattern', () => {
  assert.equal(formatResourcePattern('artifacts', '', true), 'artifacts/*');
  assert.equal(formatResourcePattern('artifacts', 'team-a', true), 'artifacts/team-a/*');
  assert.equal(formatResourcePattern('artifacts', 'team-a', false), 'artifacts/team-a');
});

test('normalizeResourcePattern', () => {
  assert.equal(normalizeResourcePattern(' artifacts//team-a/* '), 'artifacts/team-a/*');
  assert.equal(normalizeResourcePattern('artifacts/team-a*'), 'artifacts/team-a*');
  assert.equal(normalizeResourcePattern('*'), '*');
});
