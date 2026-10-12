import { expect, test } from 'vitest';
import { mergeFirstPage } from '../listingRefresh';
import { codePointCompare } from '../utils';
import type { S3Object } from '../types';

const obj = (key: string, size = 1): S3Object => ({ key, size, lastModified: '', etag: '', headers: {} });

test('a first page that holds the whole folder replaces it', () => {
  const merged = mergeFirstPage(
    { objects: [obj('a'), obj('b'), obj('z')], folders: ['m/'] },
    { objects: [obj('a', 2)], folders: [], isTruncated: false },
  );
  expect(merged.objects).toEqual([obj('a', 2)]);
  expect(merged.folders).toEqual([]);
});

test('the first page replaces entries up to its end; later pages stay', () => {
  const merged = mergeFirstPage(
    { objects: [obj('a'), obj('b'), obj('c'), obj('x'), obj('y')], folders: ['b/', 'w/'] },
    // `b` was deleted, `bb` added; the page ended at `c`.
    { objects: [obj('a'), obj('bb'), obj('c', 9)], folders: ['b/'], isTruncated: false, continuesAfter: 'c' },
  );
  expect(merged.objects.map((o) => [o.key, o.size])).toEqual([['a', 1], ['bb', 1], ['c', 9], ['x', 1], ['y', 1]]);
  expect(merged.folders).toEqual(['b/', 'w/']);
});

test('codePointCompare is S3 (UTF-8 byte) order', () => {
  // U+FF5E (3 UTF-8 bytes) sorts before U+1F600 (4 bytes); UTF-16 says the opposite.
  expect(codePointCompare('～', '\u{1F600}')).toBeLessThan(0);
  expect('～' < '\u{1F600}').toBe(false);
  expect(codePointCompare('a', 'ab')).toBeLessThan(0);
  expect(codePointCompare('ab', 'ab')).toBe(0);
});
