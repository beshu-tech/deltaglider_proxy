import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile the pure helper module to an importable data: URL. It has no runtime
// imports (only a `import type`), so it loads without a bundler or a DOM.
function dataUrl(source) {
  return `data:text/javascript;base64,${Buffer.from(source).toString('base64')}`;
}

const url = new URL('../src/components/ruleEditorHelpers.ts', import.meta.url);
const source = await readFile(url, 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ES2020,
    target: ts.ScriptTarget.ES2020,
  },
  fileName: 'ruleEditorHelpers.ts',
});

const { lineList, lines, formRow } = await import(dataUrl(outputText));

// --- lineList ----------------------------------------------------------------
assert.deepEqual(lineList('a\nb\nc'), ['a', 'b', 'c']);
assert.deepEqual(lineList('  a  \n\n  b  '), ['a', 'b']); // trimmed + blanks dropped
assert.deepEqual(lineList(''), []);
assert.deepEqual(lineList('\n  \n'), []); // whitespace-only -> empty

// --- lines (inverse on a blank-free list) ------------------------------------
assert.equal(lines(['a', 'b', 'c']), 'a\nb\nc');
assert.equal(lines([]), '');
assert.deepEqual(lineList(lines(['x/', 'y/'])), ['x/', 'y/']); // round-trip

// --- formRow -----------------------------------------------------------------
assert.deepEqual(formRow(8), { display: 'flex', alignItems: 'center', gap: 8 });
assert.deepEqual(formRow(16, { flexWrap: 'wrap', marginTop: 14 }), {
  display: 'flex',
  alignItems: 'center',
  gap: 16,
  flexWrap: 'wrap',
  marginTop: 14,
});
// extra can override the center default (column layouts)
assert.deepEqual(formRow(6, { flexDirection: 'column', alignItems: 'stretch' }), {
  display: 'flex',
  alignItems: 'stretch',
  gap: 6,
  flexDirection: 'column',
});

console.log('rule editor helpers regression checks passed');
