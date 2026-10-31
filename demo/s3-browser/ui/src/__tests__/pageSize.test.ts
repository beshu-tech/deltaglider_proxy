/**
 * Regression test for the object-table page-size selector helpers.
 *
 * Covers:
 *   - describeVisibleRange: empty, single-page, plural, thousands
 *     grouping, partial last page, size-greater-than-total fallback.
 *   - coerceStoredPageSize: null / malformed / not-in-allow-list /
 *     valid pass-through.
 */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { describeVisibleRange } from '../paginationLabels';
import { coerceStoredPageSize } from '../persistedPageSize';

const ALLOWED = [25, 50, 100, 250, 500];

test('describeVisibleRange', () => {
  // Empty listing; page/size irrelevant
  assert.equal(describeVisibleRange(0, 1, 100), 'No objects');
  assert.equal(describeVisibleRange(0, 5, 50), 'No objects');

  // The browser's nouns: objects and folders, never "items"
  assert.equal(describeVisibleRange(1, 1, 100), '1 object');
  assert.equal(describeVisibleRange(75, 1, 100), '75 objects');
  assert.equal(describeVisibleRange(100, 1, 100), '100 objects', 'exactly fills one page');
  assert.equal(describeVisibleRange(1, 1, 100, 1), '1 folder');
  assert.equal(describeVisibleRange(3, 1, 100, 3), '3 folders');
  assert.equal(describeVisibleRange(12, 1, 100, 2), '2 folders and 10 objects');
  assert.equal(describeVisibleRange(2, 1, 100, 1), '1 folder and 1 object');

  // Multi-page long form
  assert.equal(describeVisibleRange(150, 1, 100), 'Showing 1–100 of 150 objects · Page 1 of 2');
  assert.equal(describeVisibleRange(150, 2, 100), 'Showing 101–150 of 150 objects · Page 2 of 2', 'final partial page');

  // Thousands grouping (US locale — the only one our UI ships)
  assert.equal(
    describeVisibleRange(1500, 2, 100, 12),
    'Showing 101–200 of 12 folders and 1,488 objects · Page 2 of 15',
  );
  assert.equal(
    describeVisibleRange(12345, 50, 250),
    'Showing 12,251–12,345 of 12,345 objects · Page 50 of 50',
    'last page is partial; range capped at total',
  );

  // page size > total falls back to the single-page short form
  assert.equal(describeVisibleRange(5, 1, 500), '5 objects');
});

test('coerceStoredPageSize', () => {
  assert.equal(coerceStoredPageSize(null, 100, ALLOWED), 100, 'null → default');
  assert.equal(coerceStoredPageSize('not-a-number', 100, ALLOWED), 100, 'garbage → default');
  assert.equal(coerceStoredPageSize('NaN', 100, ALLOWED), 100, 'NaN string → default');
  assert.equal(coerceStoredPageSize('Infinity', 100, ALLOWED), 100, 'Infinity → default');
  assert.equal(coerceStoredPageSize('17', 100, ALLOWED), 100, 'not in allow-list → default');
  assert.equal(coerceStoredPageSize('250', 100, ALLOWED), 250, 'in allow-list passes through');
  assert.equal(coerceStoredPageSize('100', 100, ALLOWED), 100, 'default itself passes through');
});
