import assert from 'node:assert/strict';
import { afterEach, beforeEach, test, vi } from 'vitest';
import { bulkDeleteObjects, type BulkDeleteProgress } from '../adminApi/bulkObjects';
import { isSessionExpired } from '../errorHandling';
import { json, mockFetch } from '../test/fetchMock';

// Deleting a folder of 2 files showed "Deleting 0 of 2…" for 14 seconds, then
// the bar went away: the proxy answered once, after the last key. The client
// now asks for the streamed answer (one JSON line per key, then the answer)
// and reports each line.

const DELETE = '/_/api/admin/objects/delete';
const ANSWER = { deleted: 2, failed: 0, failures: [] };

/** A streamed answer whose body arrives in `chunks`. */
function ndjson(...chunks: string[]): Response {
  const enc = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      for (const chunk of chunks) c.enqueue(enc.encode(chunk));
      c.close();
    },
  });
  return new Response(stream, { status: 200, headers: { 'content-type': 'application/x-ndjson' } });
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

test('asks for the stream and reports each key, then resolves with the answer', async () => {
  http.on('POST', DELETE, () =>
    ndjson('{"done":1,"deleted":1,"fa', 'iled":0}\n{"done":2,"deleted":2,"failed":0}\n', `${JSON.stringify(ANSWER)}\n`),
  );
  const seen: BulkDeleteProgress[] = [];
  const answer = await bulkDeleteObjects({ bucket: 'releases', keys: ['a.zip', 'b.zip'] }, (p) => seen.push(p));
  assert.deepEqual(answer, ANSWER);
  assert.deepEqual(seen, [
    { done: 1, deleted: 1, failed: 0 },
    { done: 2, deleted: 2, failed: 0 },
  ]);
  const [req] = http.callsTo('POST', DELETE);
  assert.equal(req.headers.accept, 'application/x-ndjson');
  assert.deepEqual(req.body, { bucket: 'releases', keys: ['a.zip', 'b.zip'] });
});

test('an older proxy that answers plain JSON still works', async () => {
  http.on('POST', DELETE, json(ANSWER));
  const seen: BulkDeleteProgress[] = [];
  assert.deepEqual(await bulkDeleteObjects({ bucket: 'releases', keys: ['a.zip', 'b.zip'] }, (p) => seen.push(p)), ANSWER);
  assert.deepEqual(seen, []);
});

test('an error before the batch starts keeps its status', async () => {
  http.on('POST', DELETE, new Response("bucket 'releases' is temporarily read-only", { status: 409 }));
  await assert.rejects(bulkDeleteObjects({ bucket: 'releases', keys: ['a.zip'] }), /Bulk delete failed \(409\): bucket 'releases' is temporarily read-only/);
});

test('an expired session is still a session error', async () => {
  http.on('POST', DELETE, json({ error: 'unauthorized' }, 401));
  const err = await bulkDeleteObjects({ bucket: 'releases', keys: ['a.zip'] }).then(
    () => null,
    (e: unknown) => e,
  );
  assert.equal(isSessionExpired(err), true);
});

test('an error line from the proxy rejects with its message', async () => {
  http.on('POST', DELETE, () => ndjson('{"done":1,"deleted":1,"failed":0}\n{"error":"bulk delete task failed: panic"}\n'));
  await assert.rejects(bulkDeleteObjects({ bucket: 'releases', keys: ['a.zip', 'b.zip'] }), /Bulk delete failed: bulk delete task failed: panic/);
});

test('an answer that ends before its last line rejects', async () => {
  http.on('POST', DELETE, () => ndjson('{"done":1,"deleted":1,"failed":0}\n'));
  await assert.rejects(bulkDeleteObjects({ bucket: 'releases', keys: ['a.zip', 'b.zip'] }), /ended before the last line/);
});

test('a malformed line rejects', async () => {
  http.on('POST', DELETE, () => ndjson('{"done":1,"deleted":1,"failed":0}\n<html>proxy error</html>\n'));
  await assert.rejects(bulkDeleteObjects({ bucket: 'releases', keys: ['a.zip', 'b.zip'] }), /malformed line/i);
});
