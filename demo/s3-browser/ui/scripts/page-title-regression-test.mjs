/** Regression test for src/pageTitle.ts (browser-tab titles). */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const src = await readFile(new URL('../src/pageTitle.ts', import.meta.url), 'utf8');
const out = ts.transpileModule(src, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
}).outputText;
const { pageTitle } = await import(`data:text/javascript;base64,${Buffer.from(out).toString('base64')}`);

// A fresh load has no bucket yet: never a bare "— DeltaGlider Proxy".
assert.equal(pageTitle('browser', ''), 'Browse — DeltaGlider Proxy');
assert.equal(pageTitle('browser', 'releases'), 'releases — DeltaGlider Proxy');
assert.equal(pageTitle('upload', 'releases'), 'Upload to releases — DeltaGlider Proxy');
assert.equal(pageTitle('admin', '', 'Users'), 'Users · Settings — DeltaGlider Proxy');
assert.equal(pageTitle('admin', 'releases'), 'Settings — DeltaGlider Proxy');
for (const v of ['browser', 'upload', 'docs', 'admin']) {
  assert.ok(!pageTitle(v, '').startsWith('—'), `${v}: title must not start with a dash`);
}
console.log('page-title regression checks passed');
