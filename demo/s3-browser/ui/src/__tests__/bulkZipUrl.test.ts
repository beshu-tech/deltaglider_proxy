/**
 * The ZIP request URL (review A12, C6). The server reads `bucket`, `prefix`
 * and `keys` (src/api/admin/objects.rs `ZipQuery`); the GUI builds them in
 * `bulkZipDownloadUrl`.
 */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'vitest';
import { bulkZipDownloadUrl } from '../adminApi/bulkObjects';
import { zipPreflightError } from '../zipDownload';

/** The URL the GUI sends for `keys` of `bucket`. */
const zipUrl = (bucket: string, keys: string[]) => bulkZipDownloadUrl(bucket, keys);

/** The URL of the comma form that the GUI sent before B044. */
const commaUrl = (url: string, bucket: string, keys: string[]) =>
  `${url.split('?')[0]}?${new URLSearchParams({ keys: keys.map((k) => `${bucket}/${k}`).join(',') })}`;

test('A12: the largest ZIP selection is no smaller than with the comma form', () => {
  // 2,726 eight-character keys: the most the comma form fit under the limit.
  for (const folder of ['', 'releases/v1/']) {
    const keys = Array.from({ length: 2726 }, (_, i) => `${folder}f${String(i).padStart(7, '0')}`);
    const url = zipUrl('releases', keys);
    const comma = commaUrl(url, 'releases', keys);
    assert.ok(url.length <= comma.length, `${folder}: ${url.length} > ${comma.length}`);
    if (folder === '') assert.equal(zipPreflightError(keys.length, url.length), null);
  }
});

test('C6: bucket, prefix and keys give back every selected key', () => {
  const keys = ['reports/Q1, final.pdf', 'reports/a.txt', 'reports/é/ü.txt'];
  const params = new URL(zipUrl('b', keys), 'http://x').searchParams;
  const prefix = params.get('prefix') ?? '';
  assert.equal(params.get('bucket'), 'b');
  assert.equal(prefix, 'reports/');
  assert.deepEqual(
    (JSON.parse(params.get('keys') ?? '[]') as string[]).map((k) => prefix + k),
    keys,
  );
});

test('the server ZipQuery reads bucket, prefix and keys', async () => {
  const rust = await readFile(new URL('../../../../../src/api/admin/objects.rs', import.meta.url), 'utf8');
  const body = rust.split('pub struct ZipQuery {')[1]?.split('\n}')[0] ?? '';
  for (const field of ['pub bucket:', 'pub prefix:', 'pub keys:']) {
    assert.ok(body.includes(field), `ZipQuery has no ${field}`);
  }
});
