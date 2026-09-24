import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Regression guard for UPLOAD-STATS-ALWAYS-ZERO: the upload page set
// storedSize = originalSize, so "Stored" always equalled "Original size" and
// "Space saved" always read 0.0%. Stored size now comes from a HEAD after each
// upload; until every completed upload has one, the tiles show "pending".

async function load(file, rewrite = {}) {
  const source = await readFile(new URL(`../src/${file}`, import.meta.url), 'utf8');
  let { outputText } = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
    fileName: file,
  });
  for (const [from, to] of Object.entries(rewrite)) {
    outputText = outputText.replaceAll(`from '${from}'`, `from '${to}'`);
  }
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}
const { uploadSessionStats } = await import(
  await load('uploadStats.ts', { './savings': await load('savings.ts') }),
);

// empty session
assert.deepEqual(uploadSessionStats([]), { uploaded: 0, originalSize: 0, storedSize: 0, savingsPct: 0 });

// a delta upload stored 10 of 1000 bytes; a passthrough stored all 500
const done = [
  { status: 'success', originalSize: 1000, storedSize: 10 },
  { status: 'success', originalSize: 500, storedSize: 500 },
  { status: 'error', originalSize: 9999 },
  { status: 'uploading', originalSize: 9999 },
];
const s = uploadSessionStats(done);
assert.equal(s.uploaded, 2);
assert.equal(s.originalSize, 1500);
assert.equal(s.storedSize, 510, 'stored is the sum of HEAD-reported sizes, not the originals');
assert.equal(s.savingsPct, (990 / 1500) * 100);

// a completed upload whose HEAD has not landed → stored + savings unknown
const pending = uploadSessionStats([...done, { status: 'success', originalSize: 42 }]);
assert.equal(pending.uploaded, 3);
assert.equal(pending.originalSize, 1542);
assert.equal(pending.storedSize, null);
assert.equal(pending.savingsPct, null);

console.log('upload-stats regression checks passed');
