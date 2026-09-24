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
assert.deepEqual(uploadSessionStats([]), { uploaded: 0, originalSize: 0, storedSize: 0, savingsPct: 0, baselineCount: 0 });

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

// Issue #92 comment item 8: the first upload into a new folder became the
// folder's baseline. HEAD reports a 46-byte delta, but the proxy also stored
// the whole file as the reference. The page reported 99.9% saved.
const firstInFolder = uploadSessionStats([
  { status: 'success', originalSize: 3_000_000, storedSize: 46, baselineKey: 'fw/v1|aaa' },
]);
assert.equal(firstInFolder.storedSize, 3_000_046, 'the baseline bytes are stored too');
assert.equal(firstInFolder.savingsPct, 0, 'a lone baseline saves nothing');
assert.equal(firstInFolder.baselineCount, 1);

// A second version in the same folder is a real delta; the baseline counts once.
const twoVersions = uploadSessionStats([
  { status: 'success', originalSize: 3_000_000, storedSize: 46, baselineKey: 'fw/v1|aaa' },
  { status: 'success', originalSize: 3_000_000, storedSize: 30_000 },
  { status: 'success', originalSize: 3_000_000, storedSize: 46, baselineKey: 'fw/v1|aaa' },
]);
assert.equal(twoVersions.storedSize, 3_000_000 + 46 + 30_000 + 46);
assert.equal(twoVersions.baselineCount, 1);
assert.ok(twoVersions.savingsPct > 60 && twoVersions.savingsPct < 67);

const { isBaselineObject } = await import(await load('savings.ts'));
assert.equal(isBaselineObject({ 'x-amz-meta-dg-file-sha256': 'abc', 'x-amz-meta-dg-ref-sha256': 'abc' }), true);
assert.equal(isBaselineObject({ 'x-amz-meta-dg-file-sha256': 'abc', 'x-amz-meta-dg-ref-sha256': 'def' }), false);
assert.equal(isBaselineObject({}), false, 'a passthrough object has no baseline');

console.log('upload-stats regression checks passed');
