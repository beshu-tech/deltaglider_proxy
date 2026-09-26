import assert from 'node:assert/strict';
import { test } from 'vitest';
import { rowKeysFor, nextCursor, buildRows, sortRows, orderedRowKeys, sameKeyOrder, type SortState } from '../browserNav';
import type { S3Object } from '../types';

// Test fixtures only ever need `key` (and `size`/`lastModified` where noted);
// the other S3Object fields are irrelevant to the pure row/cursor logic here.
const obj = (partial: Partial<S3Object> & { key: string }): S3Object => partial as S3Object;

test('rowKeysFor: folders first (prefixed), then object keys', () => {
  const folders = ['a/', 'b/'];
  const objects = [obj({ key: 'a/x.txt' }), obj({ key: 'a/y.txt' })];
  assert.deepEqual(
    rowKeysFor(folders, objects),
    ['folder:a/', 'folder:b/', 'a/x.txt', 'a/y.txt'],
    'folders (prefixed) precede object keys, in order',
  );
  assert.deepEqual(rowKeysFor([], []), [], 'empty in, empty out');
});

const folders = ['a/', 'b/'];
const objects = [obj({ key: 'a/x.txt' }), obj({ key: 'a/y.txt' })];
const keys = rowKeysFor(folders, objects); // 4 rows

test('nextCursor: empty list -> null', () => {
  assert.equal(nextCursor([], null, 1), null, 'empty list yields null');
  assert.equal(nextCursor([], 'x', 1), null, 'empty list yields null even with a stale cursor');
});

test('nextCursor: first move from "no cursor"', () => {
  assert.equal(nextCursor(keys, null, 1), 'folder:a/', '↓ from nothing → first row');
  assert.equal(nextCursor(keys, null, -1), 'a/y.txt', '↑ from nothing → last row');
});

test('nextCursor: stepping + clamping (no wraparound)', () => {
  assert.equal(nextCursor(keys, 'folder:a/', 1), 'folder:b/', '↓ advances one');
  assert.equal(nextCursor(keys, 'folder:b/', 1), 'a/x.txt', '↓ crosses folder→object boundary');
  assert.equal(nextCursor(keys, 'folder:a/', -1), 'folder:a/', '↑ at top clamps (no wrap)');
  assert.equal(nextCursor(keys, 'a/y.txt', 1), 'a/y.txt', '↓ at bottom clamps (no wrap)');
  assert.equal(nextCursor(keys, 'a/x.txt', -1), 'folder:b/', '↑ crosses object→folder boundary');
});

test('nextCursor: Home / End', () => {
  assert.equal(nextCursor(keys, 'a/x.txt', 'first'), 'folder:a/', 'Home → first row');
  assert.equal(nextCursor(keys, 'folder:a/', 'last'), 'a/y.txt', 'End → last row');
});

test('nextCursor: stale cursor (key no longer present) behaves like "no cursor"', () => {
  assert.equal(nextCursor(keys, 'gone', 1), 'folder:a/', 'stale cursor + ↓ → first row');
  assert.equal(nextCursor(keys, 'gone', -1), 'a/y.txt', 'stale cursor + ↑ → last row');
});

test('sortRows: ONE ordering shared by Table, enrichment, cursor, keyboard', () => {
  // Regression (SORTED-TABLE-WRONG-ROWS): AntD sorted internally while enrichment,
  // the cursor page and ↑/↓ walked the UNSORTED array, so a sorted table HEADed
  // and navigated the wrong rows. sortRows must reproduce AntD's order exactly:
  // stable sort, `descend` negates the comparator (ties keep their order).
  const rows = buildRows(
    ['p/b10/', 'p/b2/'],
    [
      obj({ key: 'p/a.txt', size: 300, lastModified: '2026-01-02T00:00:00Z' }),
      obj({ key: 'p/c.txt', size: 5, lastModified: '2026-01-01T00:00:00Z' }),
      obj({ key: 'p/b3', size: 0, lastModified: '2026-01-03T00:00:00Z' }),
    ],
    'p/',
  );
  const keysOf = (rs: { key: string }[]) => rs.map((r) => r.key);
  const noSize = () => 0;
  assert.deepEqual(
    keysOf(rows),
    rowKeysFor(['p/b10/', 'p/b2/'], [obj({ key: 'p/a.txt' }), obj({ key: 'p/c.txt' }), obj({ key: 'p/b3' })]),
  );
  assert.equal(rows[0].name, 'b10/', 'names are relative to the prefix');
  assert.equal(sortRows(rows, null, noSize), rows, 'no sort → the unsorted rows, untouched');

  // AntD's reference behaviour, computed independently.
  const antd = (rs: typeof rows, cmp: (a: (typeof rows)[number], b: (typeof rows)[number]) => number, order: 'ascend' | 'descend') =>
    rs.slice().sort((x, y) => {
      const c = cmp(x, y);
      return c !== 0 ? (order === 'ascend' ? c : -c) : 0;
    });
  const numeric = (x: string, y: string) => x.localeCompare(y, undefined, { numeric: true, sensitivity: 'base' });

  // name: numeric-aware, folders interleave with files.
  assert.deepEqual(
    keysOf(sortRows(rows, { column: 'name', order: 'ascend' }, noSize)),
    ['p/a.txt', 'folder:p/b2/', 'p/b3', 'folder:p/b10/', 'p/c.txt'],
  );
  assert.deepEqual(
    keysOf(sortRows(rows, { column: 'name', order: 'descend' }, noSize)),
    keysOf(antd(rows, (x, y) => numeric(x.name, y.name), 'descend')),
  );
  // size: unscanned folders are 0 and tie with a 0-byte file — ties keep order
  // in BOTH directions (AntD negates, it does not reverse).
  assert.deepEqual(
    keysOf(sortRows(rows, { column: 'size', order: 'ascend' }, noSize)),
    ['folder:p/b10/', 'folder:p/b2/', 'p/b3', 'p/c.txt', 'p/a.txt'],
  );
  assert.deepEqual(
    keysOf(sortRows(rows, { column: 'size', order: 'descend' }, noSize)),
    ['p/a.txt', 'p/c.txt', 'folder:p/b10/', 'folder:p/b2/', 'p/b3'],
  );
  // size: a scanned folder size is used.
  assert.deepEqual(
    keysOf(sortRows(rows, { column: 'size', order: 'descend' }, (f) => (f === 'p/b2/' ? 1000 : 0))),
    ['folder:p/b2/', 'p/a.txt', 'p/c.txt', 'folder:p/b10/', 'p/b3'],
  );
  // modified: folders have no timestamp → first ascending, last descending.
  assert.deepEqual(
    keysOf(sortRows(rows, { column: 'modified', order: 'ascend' }, noSize)),
    ['folder:p/b10/', 'folder:p/b2/', 'p/c.txt', 'p/a.txt', 'p/b3'],
  );
  assert.deepEqual(
    keysOf(sortRows(rows, { column: 'modified', order: 'descend' }, noSize)),
    ['p/b3', 'p/a.txt', 'p/c.txt', 'folder:p/b10/', 'folder:p/b2/'],
  );
  // Keyboard ↓ walks the SORTED order.
  const sortedKeys = keysOf(sortRows(rows, { column: 'size', order: 'descend' } satisfies SortState, noSize));
  assert.equal(nextCursor(sortedKeys, 'p/a.txt', 1), 'p/c.txt', '↓ follows the sorted order');
});

test('sameKeyOrder: an equal report must not replace the stored order', () => {
  assert.equal(sameKeyOrder(null, []), false, 'first report is always stored');
  assert.equal(sameKeyOrder(['a', 'b'], ['a', 'b']), true);
  assert.equal(sameKeyOrder(['a', 'b'], ['b', 'a']), false);
  assert.equal(sameKeyOrder(['a'], ['a', 'b']), false);
});

test('orderedRowKeys: reported table order, or the unsorted fallback', () => {
  const rows = buildRows(
    ['p/b10/', 'p/b2/'],
    [
      obj({ key: 'p/a.txt', size: 300, lastModified: '2026-01-02T00:00:00Z' }),
      obj({ key: 'p/c.txt', size: 5, lastModified: '2026-01-01T00:00:00Z' }),
      obj({ key: 'p/b3', size: 0, lastModified: '2026-01-03T00:00:00Z' }),
    ],
    'p/',
  );
  const noSize = () => 0;
  const base = rows.map((r) => r.key);
  const sortedKeys = sortRows(rows, { column: 'size', order: 'descend' }, noSize).map((r) => r.key);
  assert.deepEqual(orderedRowKeys(base, null), base, 'nothing reported → unsorted');
  assert.deepEqual(orderedRowKeys(base, sortedKeys), sortedKeys, 'reported permutation wins');
  assert.deepEqual(orderedRowKeys(base, sortedKeys.slice(1)), base, 'stale (shorter) report → unsorted');
  assert.deepEqual(
    orderedRowKeys(base, [...sortedKeys.slice(1), 'gone']),
    base,
    'stale report naming a missing row → unsorted',
  );
});
