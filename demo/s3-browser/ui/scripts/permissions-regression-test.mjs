import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
import ts from 'typescript';

const sourceUrl = new URL('../src/permissions.ts', import.meta.url);
const source = await readFile(sourceUrl, 'utf8');
const transpiled = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ES2020,
    target: ts.ScriptTarget.ES2020,
    importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
  },
  fileName: 'permissions.ts',
}).outputText;

const moduleUrl = `data:text/javascript;base64,${Buffer.from(transpiled).toString('base64')}`;
const { canUse } = await import(moduleUrl);

const identity = {
  mode: 'iam',
  version: 'test',
  user: {
    name: 'prefix-user',
    access_key_id: 'AKIATEST',
    is_admin: false,
    permissions: [
      {
        effect: 'Allow',
        actions: ['read', 'write', 'delete'],
        resources: ['artifacts/team-a/*'],
      },
      {
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
        effect: 'Deny',
        actions: ['delete'],
        resources: ['artifacts/team-a/protected/*'],
      },
    ],
  },
};

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

const exactPrefixIdentity = {
  ...identity,
  user: {
    ...identity.user,
    permissions: [
      {
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

const openIdentity = { mode: 'open', version: 'test' };
assert.equal(canUse(openIdentity, 'write', 'anything', 'anywhere'), true);
assert.equal(canUse(null, 'read', 'artifacts', 'team-a/report.txt'), false);

console.log('permissions regression checks passed');
