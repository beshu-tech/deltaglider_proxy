import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const sourceUrl = new URL('../src/storagePath.ts', import.meta.url);
const source = await readFile(sourceUrl, 'utf8');
const transpiled = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ES2020,
    target: ts.ScriptTarget.ES2020,
    importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
  },
  fileName: 'storagePath.ts',
}).outputText;

const moduleUrl = `data:text/javascript;base64,${Buffer.from(transpiled).toString('base64')}`;
const {
  formatResourcePattern,
  normalizePrefix,
  normalizeResourcePattern,
  parseResourcePattern,
} = await import(moduleUrl);

assert.equal(normalizePrefix(' /team//${iam:username}/builds '), 'team/${iam:username}/builds/');
assert.equal(normalizePrefix(''), '');
assert.equal(normalizePrefix('///'), '');

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

assert.equal(formatResourcePattern('artifacts', '', true), 'artifacts/*');
assert.equal(formatResourcePattern('artifacts', 'team-a', true), 'artifacts/team-a/*');
assert.equal(formatResourcePattern('artifacts', 'team-a', false), 'artifacts/team-a');

assert.equal(normalizeResourcePattern(' artifacts//team-a/* '), 'artifacts/team-a/*');
assert.equal(normalizeResourcePattern('artifacts/team-a*'), 'artifacts/team-a*');
assert.equal(normalizeResourcePattern('*'), '*');

console.log('storage path regression checks passed');
