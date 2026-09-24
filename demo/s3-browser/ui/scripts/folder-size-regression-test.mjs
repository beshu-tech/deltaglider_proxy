import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/folderSize.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'folderSize.ts',
});
const { folderSizeBound, folderSizeText, folderSizeTitle } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

// Round-2 review: `sizes_estimated` / `truncated` were ignored, so an
// estimated total looked exact.
assert.equal(folderSizeBound({}), 'exact');
assert.equal(folderSizeBound({ truncated: true }), 'atLeast');
// An estimate can over-count (encrypted objects list their ciphertext size),
// so it is "about", never "at least" — even when the scan was also truncated.
assert.equal(folderSizeBound({ sizes_estimated: true }), 'about');
assert.equal(folderSizeBound({ sizes_estimated: true, truncated: true }), 'about');
// A child row uses its own estimate flag; truncation covers every child.
assert.equal(folderSizeBound({ sizes_estimated: true }, { sizes_estimated: false }), 'exact');
assert.equal(folderSizeBound({}, { sizes_estimated: true }), 'about');
assert.equal(folderSizeBound({ truncated: true }, { sizes_estimated: false }), 'atLeast');

assert.equal(folderSizeText('3.0 MB', 'exact'), '3.0 MB');
assert.equal(folderSizeText('3.0 MB', 'atLeast'), '≥ 3.0 MB');
assert.equal(folderSizeText('3.0 MB', 'about'), '≈ 3.0 MB');

assert.match(folderSizeTitle(1, 'exact'), /^1 file\. Original size/);
assert.match(folderSizeTitle(2, 'about'), /^2 files\. About this size/);
assert.match(folderSizeTitle(2, 'atLeast'), /At least this size/);
assert.doesNotMatch(folderSizeTitle(2, 'exact'), /stored \(compressed\)/);

console.log('folder-size regression tests passed');
