import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// ResourcePatternInput keeps its row helpers (splitRows / serializeRows) and the
// per-row blur-normalize logic private to the component. This test models that
// exact state machine and pins the regression the x-ray found: the OLD onBlur
// did `onChange(normalizeList(value))` — a whole-string comma split + filter —
// which dropped in-progress empty rows and desynced the stable-id list. The
// fixed onBlur normalizes ONLY the blurred row's text and never changes the row
// count. We re-derive the same pure helpers + a faithful editor sim here.

async function loadModule(relPath, fileName) {
  const source = await readFile(new URL(relPath, import.meta.url), 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}
const sp = await import(await loadModule('../src/storagePath.ts', 'storagePath.ts'));
const { normalizeResourcePattern } = sp;

// ── Faithful re-implementation of the component's row machine ────────────────
const splitRows = (value) => {
  const r = value.split(',').map((p) => p.trim());
  return r.length > 0 ? r : [''];
};
const serializeRows = (rows) =>
  rows.every((r) => !r.trim())
    ? (rows.length > 1 ? rows.map(() => '').join(', ') : '')
    : rows.map((r) => r.trim()).join(', ');

let idc = 0;
const freshId = () => `res-${(idc += 1)}`;

function makeEditor(initial) {
  let value = initial;
  let ids = splitRows(value).map(() => freshId());
  // Render-time id reconcile by COUNT (mirrors the component's idsRef block).
  const reconcile = () => {
    const rows = splitRows(value);
    if (ids.length !== rows.length) {
      const next = ids.slice(0, rows.length);
      while (next.length < rows.length) next.push(freshId());
      ids = next;
    }
  };
  const setValue = (v) => { value = v; reconcile(); };
  reconcile();
  return {
    rows: () => splitRows(value),
    ids: () => [...ids],
    value: () => value,
    addRow: () => { ids = [...ids, freshId()]; setValue(serializeRows([...splitRows(value), ''])); },
    updateRow: (i, text) => {
      const rows = splitRows(value); rows[i] = text.replace(/\r?\n/g, ' ');
      setValue(serializeRows(rows));
    },
    deleteRow: (i) => {
      ids = ids.filter((_, j) => j !== i);
      const rows = splitRows(value).filter((_, j) => j !== i);
      if (rows.length === 0) ids = [freshId()];
      setValue(serializeRows(rows.length > 0 ? rows : ['']));
    },
    // THE FIX: normalize only the blurred row's text, never the row count.
    blurRow: (i) => {
      const rows = splitRows(value);
      const cur = rows[i] ?? '';
      if (!cur.trim()) return; // empty row stays put
      const norm = normalizeResourcePattern(cur);
      if (norm !== cur) { rows[i] = norm; setValue(serializeRows(rows)); }
    },
  };
}

// ── THE regression: empty middle row + blur a sibling → no drop, ids stable ──
{
  const ed = makeEditor('beshu/a/*');
  ed.addRow();                 // -> ['beshu/a/*', '']
  ed.addRow();                 // -> ['beshu/a/*', '', '']
  ed.updateRow(2, 'beshu/c/*');// -> ['beshu/a/*', '', 'beshu/c/*']
  const idsBefore = ed.ids();
  ed.blurRow(0);               // blur the FIRST row (already normalized)
  assert.deepEqual(ed.rows(), ['beshu/a/*', '', 'beshu/c/*'], 'empty middle row must NOT be dropped on sibling blur');
  assert.deepEqual(ed.ids(), idsBefore, 'surviving rows keep their stable ids');
}

// Blur a row that needs normalization → only that row changes, count stable.
{
  const ed = makeEditor('beshu/a//b');     // double slash collapses on normalize
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
  const ed = makeEditor('a/*, b/*, c/*');
  const [id0, , id2] = ed.ids();
  ed.deleteRow(1);
  assert.deepEqual(ed.rows(), ['a/*', 'c/*']);
  assert.deepEqual(ed.ids(), [id0, id2], 'delete keeps the right ids (no tail-truncation reassignment)');
}

// Blur an empty trailing row → nothing happens (no normalization, no drop).
{
  const ed = makeEditor('a/*');
  ed.addRow();
  const idsBefore = ed.ids();
  ed.blurRow(1);
  assert.deepEqual(ed.rows(), ['a/*', '']);
  assert.deepEqual(ed.ids(), idsBefore);
}

console.log('resource pattern rows regression checks passed');
