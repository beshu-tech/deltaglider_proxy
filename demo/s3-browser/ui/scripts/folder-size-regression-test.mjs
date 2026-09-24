import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/folderSize.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'folderSize.ts',
});
const { folderSizeIsLowerBound, folderSizeText, folderSizeTitle } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

// Round-2 review: `sizes_estimated` / `truncated` were ignored, so an
// estimated total looked exact.
assert.equal(folderSizeIsLowerBound({}), false);
assert.equal(folderSizeIsLowerBound({ sizes_estimated: true }), true);
assert.equal(folderSizeIsLowerBound({ truncated: true }), true);
// A child row uses its own flag; truncation of the scan covers every child.
assert.equal(folderSizeIsLowerBound({ sizes_estimated: true }, { sizes_estimated: false }), false);
assert.equal(folderSizeIsLowerBound({}, { sizes_estimated: true }), true);
assert.equal(folderSizeIsLowerBound({ truncated: true }, { sizes_estimated: false }), true);

assert.equal(folderSizeText('3.0 MB', false), '3.0 MB');
assert.equal(folderSizeText('3.0 MB', true), '≥ 3.0 MB');

assert.match(folderSizeTitle(1, false), /^1 file\. Original size/);
assert.match(folderSizeTitle(2, true), /^2 files\. At least this size/);
assert.doesNotMatch(folderSizeTitle(2, false), /stored \(compressed\)/);

console.log('folder-size regression tests passed');
