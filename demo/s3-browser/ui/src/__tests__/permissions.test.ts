import assert from 'node:assert/strict';
import { test } from 'vitest';
import { canUse } from '../permissions';
import type { WhoamiResponse } from '../adminApi/whoami';

const identity: WhoamiResponse = {
  mode: 'iam',
  version: 'test',
  user: {
    name: 'prefix-user',
    access_key_id: 'AKIATEST',
    is_admin: false,
    permissions: [
      {
        id: 1,
        effect: 'Allow',
        actions: ['read', 'write', 'delete'],
        resources: ['artifacts/team-a/*'],
      },
      {
        id: 2,
        effect: 'Allow',
        actions: ['list'],
        resources: ['artifacts', 'artifacts/*'],
        conditions: {
          StringLike: {
            's3:prefix': ['', 'team-a/', 'team-a/*'],
          },
        },
      },
      {
        id: 3,
        effect: 'Deny',
        actions: ['delete'],
        resources: ['artifacts/team-a/protected/*'],
      },
    ],
  },
};

test('canUse: allow/deny resolution for a prefix-scoped IAM user', () => {
  assert.equal(canUse(identity, 'read', 'artifacts', 'team-a/report.txt'), true);
  assert.equal(canUse(identity, 'write', 'artifacts', 'team-a/'), true);
  assert.equal(canUse(identity, 'write', 'artifacts', 'team-a/report.txt'), true);
  assert.equal(canUse(identity, 'delete', 'artifacts', 'team-a/report.txt'), true);
  assert.equal(canUse(identity, 'delete', 'artifacts', 'team-a/protected/report.txt'), false);

  assert.equal(canUse(identity, 'read', 'artifacts', 'team-b/report.txt'), false);
  assert.equal(canUse(identity, 'write', 'artifacts', 'team-b/'), false);
  assert.equal(canUse(identity, 'delete', 'artifacts', 'team-b/report.txt'), false);

  assert.equal(canUse(identity, 'list', 'artifacts', ''), true);
  assert.equal(canUse(identity, 'list', 'artifacts', 'team-a/'), true);
  assert.equal(canUse(identity, 'list', 'artifacts', 'team-a/reports/'), true);
  assert.equal(canUse(identity, 'list', 'artifacts', 'team-b/'), false);
});

test('canUse: StringEquals s3:prefix condition is an exact match, not a prefix', () => {
  const exactPrefixIdentity: WhoamiResponse = {
    ...identity,
    user: {
      ...identity.user!,
      permissions: [
        {
          id: 1,
          effect: 'Allow',
          actions: ['list'],
          resources: ['artifacts', 'artifacts/*'],
          conditions: {
            StringEquals: {
              's3:prefix': ['team-a/'],
            },
          },
        },
      ],
    },
  };

  assert.equal(canUse(exactPrefixIdentity, 'list', 'artifacts', 'team-a/'), true);
  assert.equal(canUse(exactPrefixIdentity, 'list', 'artifacts', 'team-a/reports/'), false);
});

test('canUse: open mode allows everything; no identity denies everything', () => {
  const openIdentity: WhoamiResponse = { mode: 'open', version: 'test', user: null };
  assert.equal(canUse(openIdentity, 'write', 'anything', 'anywhere'), true);
  assert.equal(canUse(null, 'read', 'artifacts', 'team-a/report.txt'), false);
});
