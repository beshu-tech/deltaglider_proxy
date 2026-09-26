import assert from 'node:assert/strict';
import { test } from 'vitest';
import { lineList, lines, formRow } from '../components/ruleEditorHelpers';

test('lineList', () => {
  assert.deepEqual(lineList('a\nb\nc'), ['a', 'b', 'c']);
  assert.deepEqual(lineList('  a  \n\n  b  '), ['a', 'b']); // trimmed + blanks dropped
  assert.deepEqual(lineList(''), []);
  assert.deepEqual(lineList('\n  \n'), []); // whitespace-only -> empty
});

test('lines (inverse on a blank-free list)', () => {
  assert.equal(lines(['a', 'b', 'c']), 'a\nb\nc');
  assert.equal(lines([]), '');
  assert.deepEqual(lineList(lines(['x/', 'y/'])), ['x/', 'y/']); // round-trip
});

test('formRow', () => {
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
});
