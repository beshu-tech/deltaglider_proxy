import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// keys.ts has no imports, so it transpiles to a standalone data: URL.
const source = await readFile(new URL('../src/queries/keys.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'keys.ts',
});
const { qk } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);

// TanStack Query invalidation matches by key PREFIX. If `list()` is a prefix
// of a sibling key, invalidating the list also refetches the sibling (canned
// policies after every user edit; every open drawer's runs/failures after
// every jobs-list poll trigger). `all()` is the explicit root for a broad
// invalidation.
const isPrefix = (a, b) => a.length < b.length && a.every((v, i) => v === b[i]);

function familyKeys(family) {
  return Object.entries(family)
    .filter(([name]) => name !== 'all')
    .map(([name, fn]) => [name, fn('x')]);
}

for (const famName of ['users', 'jobs']) {
  const keys = familyKeys(qk[famName]);
  for (const [a, ka] of keys) {
    for (const [b, kb] of keys) {
      assert.ok(!isPrefix(ka, kb), `qk.${famName}.${a}() is a prefix of qk.${famName}.${b}()`);
    }
  }
}
assert.deepEqual(qk.users.list(), ['users', 'list']);
assert.deepEqual(qk.jobs.list(), ['jobs', 'list']);
// The root still covers every jobs key for a deliberate broad refresh.
for (const [, k] of familyKeys(qk.jobs)) assert.ok(isPrefix(qk.jobs.all(), k));

console.log('query-keys regression checks passed');
