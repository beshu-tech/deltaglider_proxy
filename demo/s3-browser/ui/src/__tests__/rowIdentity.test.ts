/**
 * F6 (epic #146): an object whose key starts with `folder:` must never be read
 * as a folder row. Folder and object rows have typed identities; the selection
 * string is an opaque encoding of one of them.
 */
/* eslint-disable no-restricted-syntax -- the test spells `folder:` on purpose: it is the F6 lookalike. */
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, test, vi } from 'vitest';
import { buildRows, rowKeysFor } from '../browserNav';
import { expandSelection } from '../bulkSelection';
import { folderRowKey, objectRowKey, parseRowKey, rowKeyOf, splitRowKeys, type RowId } from '../rowKey';
import type { S3Object } from '../types';

const obj = (key: string): S3Object => ({ key, size: 1, lastModified: '' }) as S3Object;
// An object key that spells a folder row of another encoding.
const FOLDER_LOOKALIKE = 'folder:abc/';

describe('an object named like a folder row', () => {
  test('is expanded as an object, never listed as a prefix', async () => {
    const list = vi.fn(async () => ({ keys: ['abc/x.txt'], truncated: false }));
    const keys = rowKeysFor([], [obj(FOLDER_LOOKALIKE)]);
    const items = await expandSelection(keys, list);
    expect(list).not.toHaveBeenCalled();
    expect(items.map((i) => i.source)).toEqual([FOLDER_LOOKALIKE]);
  });

  test('has a row key that differs from the folder row of the same text', () => {
    const objectRow = rowKeysFor([], [obj(FOLDER_LOOKALIKE)])[0];
    const folderRow = rowKeysFor(['abc/'], [])[0];
    expect(objectRow).not.toBe(folderRow);
  });

  test('a folder and an object of the same text are two rows with two keys', () => {
    const rows = buildRows(['abc/'], [obj('abc/')], '');
    expect(new Set(rows.map((r) => r.rowKey)).size).toBe(2);
  });

  test('is split as an object key for a delete', () => {
    expect(splitRowKeys([objectRowKey(FOLDER_LOOKALIKE), folderRowKey('abc/')])).toEqual({
      prefixes: ['abc/'],
      keys: [FOLDER_LOOKALIKE],
    });
  });
});

describe('row keys over arbitrary text', () => {
  // A seeded generator: the property runs are the same on every machine.
  let seed = 0x9e3779b9;
  const next = () => {
    seed = (Math.imul(seed ^ (seed >>> 15), 0x85ebca6b) + 0x6d2b79f5) | 0;
    return (seed >>> 0) / 2 ** 32;
  };
  const ALPHABET = ['a', 'b', '/', ':', 'd', 'o', 'f', 'l', 'e', 'r', ' ', '\u00e9', '\u{1F600}', '.'];
  const text = () => {
    if (next() < 0.2) return ['folder:', 'd:', 'o:', 'folder:o:', ''][Math.floor(next() * 5)] + (next() < 0.5 ? 'abc/' : '');
    return Array.from({ length: Math.floor(next() * 12) }, () => ALPHABET[Math.floor(next() * ALPHABET.length)]).join('');
  };
  const rowId = (): RowId => (next() < 0.5 ? { kind: 'folder', prefix: text() } : { kind: 'object', key: text() });
  const RUNS = 2000;

  test('parse inverts make', () => {
    for (let i = 0; i < RUNS; i++) {
      const id = rowId();
      expect(parseRowKey(rowKeyOf(id))).toEqual(id);
    }
  });

  test('two different identities never share a row key', () => {
    for (let i = 0; i < RUNS; i++) {
      const [a, b] = [rowId(), rowId()];
      expect(rowKeyOf(a) === rowKeyOf(b)).toBe(JSON.stringify(a) === JSON.stringify(b));
    }
  });

  test('an object key is never read as a folder, whatever its text', () => {
    for (let i = 0; i < RUNS; i++) {
      const t = text();
      expect(parseRowKey(objectRowKey(t)).kind).toBe('object');
      expect(parseRowKey(objectRowKey(`folder:${t}`)).kind).toBe('object');
    }
  });
});

/** Source guard: only src/rowKey.ts builds or takes apart row-key text. */
describe('source guard', () => {
  function sources(dir: string): string[] {
    return readdirSync(dir).flatMap((name) => {
      const path = join(dir, name);
      if (statSync(path).isDirectory()) return name === '__tests__' || name === 'test' ? [] : sources(path);
      return /\.(ts|tsx)$/.test(name) && name !== 'rowKey.ts' ? [path] : [];
    });
  }
  const FORBIDDEN: [RegExp, string][] = [
    [/['"`]folder:/, 'a `folder:` string (build row keys with folderRowKey)'],
    [/\bas\s+(?:unknown\s+as\s+)?RowKey\b/, 'a cast to RowKey (only rowKey.ts makes one)'],
    [/\.(startsWith|slice|replace)\(\s*['"`][do]:['"`]/, 'parsing a row key by its tag (use parseRowKey)'],
  ];

  test.each(sources(join(__dirname, '..')))('%s makes no row key by hand', (file) => {
    const text = readFileSync(file, 'utf8');
    for (const [re, what] of FORBIDDEN) {
      const hit = re.exec(text);
      expect(hit, `${file} has ${what}: ${hit?.[0]}`).toBeNull();
    }
  });
});
