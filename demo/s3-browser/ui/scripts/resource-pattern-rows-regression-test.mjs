import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// ResourcePatternInput now stores `resources` as a STRING ARRAY end-to-end
// (matching the server model), via the pure helpers in resourcePatternRows.ts.
// The OLD editor flattened the array to a comma-joined string and split it back
// on save — silently corrupting any pattern containing a literal `,` (valid in
// an S3 key) and desyncing row ids via count-based reconciliation. This test
// imports the REAL helpers and models the component's array-based row machine
// to pin: (1) a comma-bearing pattern survives load→edit→save unchanged; (2)
// surviving rows keep their stable ids through blur/delete; (3) blur normalizes
// only the blurred row, never the row count.

async function loadModule(relPath, fileName, replaceImports = {}) {
  let source = await readFile(new URL(relPath, import.meta.url), 'utf8');
  for (const [spec, dataUrl] of Object.entries(replaceImports)) {
    source = source.replaceAll(`'${spec}'`, `'${dataUrl}'`);
  }
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ES2020,
      target: ts.ScriptTarget.ES2020,
      importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
    },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}
const storagePathUrl = await loadModule('../src/storagePath.ts', 'storagePath.ts');
const rowsUrl = await loadModule('../src/resourcePatternRows.ts', 'resourcePatternRows.ts', {
  './storagePath': storagePathUrl,
});
const { parseResourceRows, serializeResourceRows, freshResourceRowId, normalizeResourceRowPattern } =
  await import(rowsUrl);

// ── Faithful re-implementation of the component's array-based row machine ─────
function makeEditor(initialArray) {
  let rows = parseResourceRows(initialArray);
  const emit = (mutate) => { rows = mutate(rows); };
  return {
    rows: () => rows.map((r) => r.text),
    ids: () => rows.map((r) => r.id),
    value: () => serializeResourceRows(rows), // the emitted string[]
    addRow: () => { rows = [...rows, { id: freshResourceRowId(), text: '' }]; },
    updateRow: (i, text) =>
      emit((cur) => cur.map((r, j) => (j === i ? { ...r, text: text.replace(/\r?\n/g, ' ') } : r))),
    deleteRow: (i) =>
      emit((cur) => {
        const remaining = cur.filter((_, j) => j !== i);
        return remaining.length > 0 ? remaining : [{ id: freshResourceRowId(), text: '' }];
      }),
    blurRow: (i) =>
      emit((cur) =>
        cur.map((r, j) => {
          if (j !== i || !r.text.trim()) return r;
          return { ...r, text: normalizeResourceRowPattern(r.text) };
        }),
      ),
  };
}

// ── THE data-loss regression: a comma-bearing pattern is ONE entry ───────────
{
  // An S3 key can contain a comma. The OLD editor split "bucket/a,b/*" into
  // two bogus patterns. The array model must keep it intact through a full
  // load → serialize round-trip.
  const withComma = ['releases/a,b/*', 'db-archive/*'];
  const ed = makeEditor(withComma);
  assert.deepEqual(ed.value(), withComma, 'comma-bearing pattern survives load→save unchanged');
  assert.deepEqual(ed.rows(), ['releases/a,b/*', 'db-archive/*'], 'comma is not a delimiter');

  // Edit a sibling and re-serialize — the comma row is still one entry.
  ed.updateRow(1, 'db-archive/2026,q1/*');
  assert.deepEqual(
    ed.value(),
    ['releases/a,b/*', 'db-archive/2026,q1/*'],
    'commas in BOTH rows stay intact after an edit',
  );
}

// ── Empty middle row + blur a sibling → no drop, ids stable ──────────────────
{
  const ed = makeEditor(['beshu/a/*']);
  ed.addRow();                  // -> ['beshu/a/*', '']
  ed.addRow();                  // -> ['beshu/a/*', '', '']
  ed.updateRow(2, 'beshu/c/*'); // -> ['beshu/a/*', '', 'beshu/c/*']
  const idsBefore = ed.ids();
  ed.blurRow(0);                // blur the FIRST row (already normalized)
  assert.deepEqual(ed.rows(), ['beshu/a/*', '', 'beshu/c/*'], 'empty middle row must NOT be dropped on sibling blur');
  assert.deepEqual(ed.ids(), idsBefore, 'surviving rows keep their stable ids');
  // The emitted value drops the blank (not persistable) but keeps both reals.
  assert.deepEqual(ed.value(), ['beshu/a/*', 'beshu/c/*'], 'serialize drops blanks, keeps order');
}

// Blur a row that needs normalization → only that row changes, count stable.
{
  const ed = makeEditor(['beshu/a//b']); // double slash collapses on normalize
  ed.addRow();
  ed.updateRow(1, 'beshu/keep/*');
  const idsBefore = ed.ids();
  ed.blurRow(0);
  assert.equal(ed.rows()[0], 'beshu/a/b', 'blurred row normalized in place');
  assert.equal(ed.rows()[1], 'beshu/keep/*', 'sibling untouched');
  assert.deepEqual(ed.ids(), idsBefore, 'ids stable through in-place normalize');
}

// Delete the middle row → surviving rows keep their ORIGINAL ids (no shift).
{
  const ed = makeEditor(['a/*', 'b/*', 'c/*']);
  const [id0, , id2] = ed.ids();
  ed.deleteRow(1);
  assert.deepEqual(ed.rows(), ['a/*', 'c/*']);
  assert.deepEqual(ed.ids(), [id0, id2], 'delete keeps the right ids (no tail-truncation reassignment)');
}

// Blur an empty trailing row → nothing happens (no normalization, no drop).
{
  const ed = makeEditor(['a/*']);
  ed.addRow();
  const idsBefore = ed.ids();
  ed.blurRow(1);
  assert.deepEqual(ed.rows(), ['a/*', '']);
  assert.deepEqual(ed.ids(), idsBefore);
}

console.log('resource-pattern-rows regression: OK');
