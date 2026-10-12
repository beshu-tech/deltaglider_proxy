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
  // Empty listing
  assert.equal(describeVisibleRange(0, 1, 100), '0 items');
  assert.equal(describeVisibleRange(0, 5, 50), '0 items', 'page/size irrelevant when empty');

  // Singular grammar
  assert.equal(describeVisibleRange(1, 1, 100), '1 item');

  // Single-page short form (no page numbers, no Showing-X–Y)
  assert.equal(describeVisibleRange(75, 1, 100), '75 items');
  assert.equal(describeVisibleRange(100, 1, 100), '100 items', 'exactly fills one page');

  // Multi-page long form
  assert.equal(
    describeVisibleRange(150, 1, 100),
    'Showing 1–100 of 150 items · Page 1 of 2',
    'first page of two',
  );
  assert.equal(
    describeVisibleRange(150, 2, 100),
    'Showing 101–150 of 150 items · Page 2 of 2',
    'final partial page',
  );

  // Thousands grouping (US locale — the only one our UI ships)
  assert.equal(
    describeVisibleRange(1500, 2, 100),
    'Showing 101–200 of 1,500 items · Page 2 of 15',
  );
  assert.equal(
    describeVisibleRange(12345, 50, 250),
    'Showing 12,251–12,345 of 12,345 items · Page 50 of 50',
    'last page is partial; range capped at total',
  );

  // Page boundary safety: page=1 with size>total still summarises sanely.
  assert.equal(
    describeVisibleRange(5, 1, 500),
    '5 items',
    'when page size > total, falls back to single-page short form',
  );
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
