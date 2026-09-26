import assert from 'node:assert/strict';
import { test } from 'vitest';
import { writablePrefixesForBucket, virtualWritableChildren } from '../permissions';
import type { WhoamiResponse } from '../adminApi';

const identity: WhoamiResponse = {
  mode: 'iam',
  version: 'test',
  user: {
    name: 'writer',
    access_key_id: 'AKIATEST',
    is_admin: false,
    permissions: [
      {
        id: 1,
        effect: 'Allow',
        actions: ['write'],
        resources: ['artifacts/team-a/*', 'artifacts/team-b/sub/*'],
      },
      {
        id: 2,
        effect: 'Deny',
        actions: ['write'],
        resources: ['artifacts/team-a/private/*'],
      },
    ],
  },
};

test('writablePrefixesForBucket', () => {
  assert.deepEqual(writablePrefixesForBucket(identity, 'artifacts'), ['team-a/', 'team-b/sub/']);
  assert.deepEqual(writablePrefixesForBucket(identity, 'other-bucket'), []);
});

test('writablePrefixesForBucket: overlapping allow/deny resources', () => {
  const overlappingIdentity: WhoamiResponse = {
    mode: 'iam',
    version: 'test',
    user: {
      name: 'writer-overlap',
      access_key_id: 'AKIATEST2',
      is_admin: false,
      permissions: [
        {
          id: 1,
          effect: 'Allow',
          actions: ['write'],
          resources: [
            'artifacts/team-c/*',
            'artifacts/team-c/releases/*',
            'artifacts/team-c/releases/canary/*',
            'artifacts/team-d/raw/*',
          ],
        },
        {
          id: 2,
          effect: 'Deny',
          actions: ['write'],
          resources: ['artifacts/team-c/releases/*'],
        },
        {
          id: 3,
          effect: 'Allow',
          actions: ['s3:PutObject'],
          resources: ['artifacts/team-e/nested/*'],
        },
        {
          id: 4,
          effect: 'Allow',
          actions: ['write'],
          resources: ['other/team-z/*'],
        },
      ],
    },
  };
  assert.deepEqual(
    writablePrefixesForBucket(overlappingIdentity, 'artifacts'),
    ['team-c/', 'team-d/raw/', 'team-e/nested/'],
  );
});

test('virtualWritableChildren', () => {
  const rootVirtual = virtualWritableChildren('', ['logs/'], ['team-a/', 'team-b/sub/']);
  assert.deepEqual(rootVirtual, ['team-a/', 'team-b/']);

  const teamBVirtual = virtualWritableChildren('team-b/', [], ['team-a/', 'team-b/sub/']);
  assert.deepEqual(teamBVirtual, ['team-b/sub/']);

  const noDuplicateWhenRealExists = virtualWritableChildren('', ['team-a/'], ['team-a/', 'team-b/sub/']);
  assert.deepEqual(noDuplicateWhenRealExists, ['team-b/']);

  const deepVirtual = virtualWritableChildren(
    'team-c/',
    ['team-c/archive/'],
    ['team-c/', 'team-c/releases/canary/', 'team-c/releases/stable/', 'team-c/archive/'],
  );
  assert.deepEqual(deepVirtual, ['team-c/releases/']);

  const deeperVirtual = virtualWritableChildren(
    'team-c/releases/',
    ['team-c/releases/stable/'],
    ['team-c/', 'team-c/releases/canary/', 'team-c/releases/stable/', 'team-c/releases/edge/nightly/'],
  );
  assert.deepEqual(deeperVirtual, ['team-c/releases/canary/', 'team-c/releases/edge/']);

  const nonDescendantVirtual = virtualWritableChildren(
    'team-z/',
    [],
    ['team-a/', 'team-z/logs/day-1/', 'team-z/logs/day-2/', 'team-z/'],
  );
  assert.deepEqual(nonDescendantVirtual, ['team-z/logs/']);
});
