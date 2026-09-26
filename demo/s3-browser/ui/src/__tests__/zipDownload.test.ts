/**
 * Regression guard for issue #92 comment item 6: a bulk ZIP the server would
 * refuse (too many keys, a request line above 64 KiB) used to start an anchor
 * download that silently failed. The preflight now names the reason first.
 */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'vitest';
import { zipPreflightError, downloadZip, type ZipDownloadDeps } from '../zipDownload';
import { ApiError, isSessionExpired } from '../errorHandling';

// The limits mirror the server constants in src/api/admin/objects.rs.
const ZIP_MAX_KEYS = 10_000;
const ZIP_MAX_URL_LENGTH = 60_000;

test('mirrored constants match src/zipDownload.ts and src/api/admin/objects.rs', async () => {
  const src = await readFile(new URL('../zipDownload.ts', import.meta.url), 'utf8');
  assert.match(src, /const ZIP_MAX_KEYS = 10_000;/);
  assert.match(src, /const ZIP_MAX_URL_LENGTH = 60_000;/);
  const rust = await readFile(new URL('../../../../../src/api/admin/objects.rs', import.meta.url), 'utf8');
  assert.match(
    rust,
    new RegExp(`const MAX_BULK_OBJECTS: usize = ${ZIP_MAX_KEYS.toLocaleString('en-US').replace(/,/g, '_')};`),
  );
  // The server streams the ZIP: no archive size cap on either side.
  assert.doesNotMatch(rust, /MAX_ZIP_BYTES/);
  assert.doesNotMatch(src, /ZIP_MAX_BYTES/);
});

test('zipPreflightError', () => {
  assert.equal(zipPreflightError(1, 100), null);
  assert.equal(zipPreflightError(ZIP_MAX_KEYS, ZIP_MAX_URL_LENGTH), null);
  assert.match(zipPreflightError(0, 50) ?? '', /no files/);
  assert.match(zipPreflightError(ZIP_MAX_KEYS + 1, 100) ?? '', /10,001 files.*at most 10,000/);
  assert.match(zipPreflightError(900, ZIP_MAX_URL_LENGTH + 1) ?? '', /too many files \(900\)/);
});

interface FakeHandle {
  createWritable: () => Promise<WritableStream<Uint8Array>>;
  getFile?: () => Promise<{ size: number }>;
  remove?: () => Promise<void>;
}

interface Fakes {
  calls: { removed: number; wrote: number };
  deps: ZipDownloadDeps;
}

// --- downloadZip, non-2xx: the picked file is deleted, the error explains ---
function fakes(status: number, body = '', sizeBefore = 0): Fakes {
  const calls = { removed: 0, wrote: 0 };
  const handle: FakeHandle = {
    createWritable: async () => { calls.wrote++; return new WritableStream(); },
    getFile: async () => ({ size: sizeBefore }),
    remove: async () => { calls.removed++; },
  };
  const deps: ZipDownloadDeps = {
    picker: async () => handle,
    fetch: (async () => new Response(status === 200 ? 'PK' : body, {
      status,
      headers: { 'content-type': 'application/json' },
    })) as typeof fetch,
  };
  return { calls, deps };
}

test('413 (a file too large to read): the empty file is deleted, the server message shows', async () => {
  const { calls, deps } = fakes(413, JSON.stringify({ error: 'object too large' }));
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), /object too large/);
  assert.equal(calls.removed, 1, '413: the empty file is deleted');
  assert.equal(calls.wrote, 0);
});

// U4: the server streams the archive and aborts the response when a file
// fails after its bytes started. The transfer must fail visibly, and a new
// file must not stay behind half-written.
test('a transfer that breaks mid-stream: the new file is deleted, the error passes through', async () => {
  for (const sizeBefore of [0, 4096]) {
    const { calls, deps } = fakes(200, '', sizeBefore);
    deps.fetch = (async () => new Response(new ReadableStream<Uint8Array>({
      start(c) {
        c.enqueue(new TextEncoder().encode('PK\x03\x04 partial'));
        c.error(new TypeError('network error'));
      },
    }), { status: 200 })) as typeof fetch;
    await assert.rejects(downloadZip('/zip', 'a.zip', deps), /network error/);
    assert.equal(calls.wrote, 1);
    assert.equal(calls.removed, sizeBefore === 0 ? 1 : 0, `sizeBefore=${sizeBefore}`);
  }
});

test('404: the empty file is deleted, ApiError carries the server message', async () => {
  const { calls, deps } = fakes(404, JSON.stringify({ error: 'None of the selected files could be read.' }));
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), (e: unknown) => {
    assert.ok(e instanceof ApiError);
    assert.equal(e.status, 404);
    assert.match(e.message, /None of the selected files could be read/);
    return true;
  });
  assert.equal(calls.removed, 1, '404: the empty file is deleted');
});

test('401: reads as an expired session', async () => {
  const { calls, deps } = fakes(401, JSON.stringify({ error: 'unauthorized' }));
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), (e: unknown) => isSessionExpired(e));
  assert.equal(calls.removed, 1);
});

test('network failure: the file is deleted and the error passes through', async () => {
  const { calls, deps } = fakes(200);
  deps.fetch = (async () => { throw new TypeError('Failed to fetch'); }) as typeof fetch;
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), /Failed to fetch/);
  assert.equal(calls.removed, 1);
});

test('an existing file the user picked to overwrite: a failure leaves it alone', async () => {
  for (const status of [413, 404, 502]) {
    const { calls, deps } = fakes(status, '{}', 4096);
    await assert.rejects(downloadZip('/zip', 'old.zip', deps));
    assert.equal(calls.removed, 0, `${status}: an existing file is never deleted`);
    assert.equal(calls.wrote, 0, `${status}: nothing is written to it`);
  }
  // Size unknown (no getFile): do not delete either.
  const { calls, deps } = fakes(413);
  const h = (await deps.picker?.({})) as FakeHandle;
  delete h.getFile;
  deps.picker = async () => h;
  await assert.rejects(downloadZip('/zip', 'a.zip', deps));
  assert.equal(calls.removed, 0, 'unknown size: keep the file');
});

test('success keeps the file', async () => {
  const { calls, deps } = fakes(200);
  assert.equal(await downloadZip('/zip', 'a.zip', deps), 'saved');
  assert.equal(calls.removed, 0, 'success keeps the file');
  assert.equal(calls.wrote, 1);
});

test('a browser without remove() still gets the error', async () => {
  const { deps } = fakes(502);
  const h = (await deps.picker?.({})) as FakeHandle;
  delete h.remove;
  deps.picker = async () => h;
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), /ZIP download failed \(502\)/);
});
