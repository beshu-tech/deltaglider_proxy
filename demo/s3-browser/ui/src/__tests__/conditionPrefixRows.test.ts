import assert from 'node:assert/strict';
import { test } from 'vitest';
import { parseRowsArray, serializeRowsArray, normalizePrefixPattern, freshRowId, type PrefixRow } from '../conditionPrefixRows';

const texts = (rows: PrefixRow[]) => rows.map((r) => r.text);
// Test-harness adapters: seed rows from a comma string and read the serialized
// non-root output back as a comma string. The COMPONENT uses the array helpers
// (parseRowsArray/serializeRowsArray); these wrappers keep the legacy
// edit-lifecycle assertions readable without re-exporting dead string helpers.
const parseRows = (s: string) => parseRowsArray(s.split(',').map((p) => p.trim())).rows;
const serializeRows = (rows: PrefixRow[]) => serializeRowsArray(rows, false).join(', ');
const mk = (...t: string[]): PrefixRow[] => t.map((text) => ({ id: freshRowId(), text }));

test('normalizePrefixPattern', () => {
  assert.equal(normalizePrefixPattern('uploads/*'), 'uploads/*');
  assert.equal(normalizePrefixPattern('ror//builds/*'), 'ror/builds/*'); // collapse double slash
  assert.equal(normalizePrefixPattern('*'), '*');
  assert.equal(normalizePrefixPattern('.*'), '.*');
  assert.equal(normalizePrefixPattern(''), '');

  // THE bug: a trailing slash must NOT be auto-appended on blur, because for an
  // `s3:prefix StringLike` condition `ror/libs` and `ror/libs/` are NOT
  // equivalent — the slash-less form also matches `ror/libs-internal/…`.
  assert.equal(normalizePrefixPattern('ror/libs'), 'ror/libs', 'no trailing slash forced');
  assert.equal(normalizePrefixPattern('ror/libs/'), 'ror/libs/', 'explicit trailing slash kept');
  assert.equal(normalizePrefixPattern(' ror/libs '), 'ror/libs', 'trimmed, still no slash');
  assert.equal(normalizePrefixPattern('ror//libs'), 'ror/libs', 'collapse // but no trailing slash');
  assert.equal(normalizePrefixPattern('ror\\libs'), 'ror/libs', 'backslash → slash, no trailing slash');
  // Wildcard forms preserve the operator's slash choice too.
  assert.equal(normalizePrefixPattern('ror/libs*'), 'ror/libs*');
  assert.equal(normalizePrefixPattern('ror/libs/*'), 'ror/libs/*');
});

test('parseRows (harness over parseRowsArray)', () => {
  assert.deepEqual(texts(parseRows('uploads/*, ror/, ror/builds/, ror/e2e_reports/')), [
    'uploads/*',
    'ror/',
    'ror/builds/',
    'ror/e2e_reports/',
  ]);
  assert.deepEqual(texts(parseRows('')), ['']); // empty input → one blank editable row

  // stable, unique ids
  const ids = parseRows('a, b, c').map((r) => r.id);
  assert.equal(new Set(ids).size, 3);
  assert.notEqual(freshRowId(), freshRowId());
});

test('serializeRows (harness over serializeRowsArray)', () => {
  assert.equal(serializeRows(mk('uploads/*', 'ror/')), 'uploads/*, ror/');
  assert.equal(serializeRows(mk('uploads/*', '', 'ror/')), 'uploads/*, ror/'); // empty rows dropped
  assert.equal(serializeRows(mk('', '')), ''); // all-empty -> empty string, no dangling commas
  assert.equal(serializeRows(mk(' a ', ' b ')), 'a, b'); // trimmed
});

test('Array contract: root-prefix ("") preservation (Obstacle 1)', () => {
  // The persisted s3:prefix is a string[] where "" means "list bucket root".
  // parseRowsArray splits root into a boolean; serializeRowsArray recombines it,
  // emitting "" FIRST. This is the fix for the empty-prefix being silently
  // dropped by the old comma-split round-trip.

  // Parse: "" becomes includeRoot=true and is NOT a text row.
  const a = parseRowsArray(['', 'ror/libs/', 'ror/libs/*']);
  assert.equal(a.includeRoot, true);
  assert.deepEqual(texts(a.rows), ['ror/libs/', 'ror/libs/*']);

  // Parse with no root.
  const b = parseRowsArray(['ror/libs/']);
  assert.equal(b.includeRoot, false);
  assert.deepEqual(texts(b.rows), ['ror/libs/']);

  // Empty input → one blank editable row, no root.
  const c = parseRowsArray([]);
  assert.equal(c.includeRoot, false);
  assert.deepEqual(texts(c.rows), ['']);

  // Serialize: root emitted first, blanks dropped, de-duped, order preserved.
  assert.deepEqual(serializeRowsArray(mk('ror/libs/', 'ror/libs/*'), true), ['', 'ror/libs/', 'ror/libs/*']);
  assert.deepEqual(serializeRowsArray(mk('ror/libs/', '', 'ror/builds/'), false), ['ror/libs/', 'ror/builds/']);
  assert.deepEqual(serializeRowsArray(mk('a', 'a', 'b'), false), ['a', 'b']); // de-dupe
  assert.deepEqual(serializeRowsArray(mk('', ''), false), []); // all-blank, no root → empty (drops condition)
  assert.deepEqual(serializeRowsArray(mk('', ''), true), ['']); // root only

  // Round-trip: ["", "ror/"] survives intact (the case the OLD code collapsed).
  const seeded = parseRowsArray(['', 'ror/']);
  assert.deepEqual(serializeRowsArray(seeded.rows, seeded.includeRoot), ['', 'ror/']);
});

// --- Full edit-lifecycle simulation (THE regression) -------------------------
// Models ConditionPrefixInput's local-state machine: rows live in state keyed
// by stable id; the comma string is only an OUTPUT. The topmost row must NEVER
// disappear when a newly-added row is typed into and then blurred.
function makeEditor(initial: string) {
  let rows = parseRows(initial);
  let lastEmitted = serializeRows(rows);
  let emitted = lastEmitted;
  const emit = (mutate: (rows: PrefixRow[]) => PrefixRow[]) => {
    rows = mutate(rows);
    const s = serializeRows(rows);
    if (s !== lastEmitted) {
      lastEmitted = s;
      emitted = s;
    }
  };
  return {
    rows: () => rows,
    emitted: () => emitted,
    addRow: () => {
      rows = [...rows, { id: freshRowId(), text: '' }];
    },
    updateRow: (i: number, text: string) => emit((cur) => cur.map((r, idx) => (idx === i ? { ...r, text } : r))),
    blurRow: (i: number) =>
      emit((cur) => cur.map((r, idx) => (idx === i ? { ...r, text: normalizePrefixPattern(r.text) } : r))),
    deleteRow: (i: number) =>
      emit((cur) => {
        const remaining = cur.filter((_, idx) => idx !== i);
        return remaining.length > 0 ? remaining : [{ id: freshRowId(), text: '' }];
      }),
  };
}

test('exact repro from the bug report', () => {
  const ed = makeEditor('uploads/*, ror/, ror/builds/, ror/e2e_reports/');
  ed.addRow(); // "+ Add prefix"
  assert.deepEqual(texts(ed.rows()), ['uploads/*', 'ror/', 'ror/builds/', 'ror/e2e_reports/', '']);
  ed.updateRow(4, 'newprefix'); // type into the new row
  ed.blurRow(4); // click outside (blur)
  assert.equal(ed.rows()[0].text, 'uploads/*', 'topmost row must survive blur of another row');
  // Blur must NOT append a trailing slash (would change s3:prefix semantics).
  assert.deepEqual(texts(ed.rows()), ['uploads/*', 'ror/', 'ror/builds/', 'ror/e2e_reports/', 'newprefix']);
  assert.equal(ed.emitted(), 'uploads/*, ror/, ror/builds/, ror/e2e_reports/, newprefix');
});

test('the exact case from the bug report: "ror/libs" (no slash) stays as-is on blur', () => {
  const ed = makeEditor('ror/, ror/builds/, ror/e2e_reports/, ror/libs/');
  ed.addRow();
  ed.updateRow(4, 'ror/libs');
  ed.blurRow(4);
  assert.deepEqual(texts(ed.rows()), ['ror/', 'ror/builds/', 'ror/e2e_reports/', 'ror/libs/', 'ror/libs']);
  assert.equal(ed.emitted(), 'ror/, ror/builds/, ror/e2e_reports/, ror/libs/, ror/libs');
});

test('add a row and blur WITHOUT typing — no existing row may vanish', () => {
  const ed = makeEditor('uploads/*, ror/');
  ed.addRow();
  ed.blurRow(2);
  assert.deepEqual(texts(ed.rows()).slice(0, 2), ['uploads/*', 'ror/']);
  assert.equal(ed.emitted(), 'uploads/*, ror/');
});

test('blur the FIRST row — must not touch the others', () => {
  const ed = makeEditor('uploads/*, ror/, ror/builds/');
  ed.blurRow(0);
  assert.deepEqual(texts(ed.rows()), ['uploads/*', 'ror/', 'ror/builds/']);
});

test('delete the middle row — neighbors intact', () => {
  const ed = makeEditor('uploads/*, ror/, ror/builds/');
  ed.deleteRow(1);
  assert.deepEqual(texts(ed.rows()), ['uploads/*', 'ror/builds/']);
  assert.equal(ed.emitted(), 'uploads/*, ror/builds/');
});

test('edit several rows in a burst (functional updaters build on latest state)', () => {
  const ed = makeEditor('a/, b/, c/');
  ed.updateRow(0, 'x/');
  ed.updateRow(2, 'z/');
  assert.deepEqual(texts(ed.rows()), ['x/', 'b/', 'z/']);
  assert.equal(ed.emitted(), 'x/, b/, z/');
});
