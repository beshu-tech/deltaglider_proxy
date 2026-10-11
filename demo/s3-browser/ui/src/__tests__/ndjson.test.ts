import assert from 'node:assert/strict';
import { test } from 'vitest';
import { readNdjson } from '../ndjson';

// The bulk delete answers with one JSON line per deleted key, then the
// answer (adminApi/bulkObjects.ts). The network cuts that body into chunks
// anywhere: inside a line, inside a UTF-8 character, or many lines at once.

/** A body that hands out `chunks` one read at a time. */
function body(...chunks: (string | Uint8Array)[]): ReadableStream<Uint8Array> {
  const enc = new TextEncoder();
  return new ReadableStream({
    start(c) {
      for (const chunk of chunks) c.enqueue(typeof chunk === 'string' ? enc.encode(chunk) : chunk);
      c.close();
    },
  });
}

async function read(stream: ReadableStream<Uint8Array>): Promise<unknown[]> {
  const lines: unknown[] = [];
  await readNdjson(stream, (line) => lines.push(line));
  return lines;
}

test('a line cut across chunks is read once it is whole', async () => {
  assert.deepEqual(await read(body('{"done":1,"del', 'eted":1}\n{"done":2', ',"deleted":2}\n')), [
    { done: 1, deleted: 1 },
    { done: 2, deleted: 2 },
  ]);
});

test('several lines in one chunk each count, in order', async () => {
  assert.deepEqual(await read(body('{"n":1}\n{"n":2}\n{"n":3}\n')), [{ n: 1 }, { n: 2 }, { n: 3 }]);
});

test('a UTF-8 character cut between two chunks is decoded whole', async () => {
  const bytes = new TextEncoder().encode('{"key":"é"}\n');
  const cut = bytes.indexOf(0xc3) + 1; // inside the two bytes of é
  assert.deepEqual(await read(body(bytes.slice(0, cut), bytes.slice(cut))), [{ key: 'é' }]);
});

test('the final line counts without a trailing newline; blank lines and CRLF are ignored', async () => {
  assert.deepEqual(await read(body('{"n":1}\r\n\n', '{"deleted":2,"failed":0,"failures":[]}')), [
    { n: 1 },
    { deleted: 2, failed: 0, failures: [] },
  ]);
  assert.deepEqual(await read(body()), []);
});

test('each line reaches the caller before the next chunk arrives', async () => {
  const enc = new TextEncoder();
  let push!: (s: string | null) => void;
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      push = (s) => (s === null ? c.close() : c.enqueue(enc.encode(s)));
    },
  });
  const seen: unknown[] = [];
  const run = readNdjson(stream, (line) => seen.push(line));
  push('{"done":1}\n');
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(seen, [{ done: 1 }], 'the first line arrived while the body is still open');
  push('{"done":2}\n');
  push(null);
  await run;
  assert.deepEqual(seen, [{ done: 1 }, { done: 2 }]);
});

test('a malformed line rejects and stops the read', async () => {
  const seen: unknown[] = [];
  await assert.rejects(
    readNdjson(body('{"n":1}\n', 'not json\n{"n":2}\n'), (line) => seen.push(line)),
    /malformed line in the answer: not json/i,
  );
  assert.deepEqual(seen, [{ n: 1 }]);
});

test('an error that the caller throws for a line stops the read with that error', async () => {
  const boom = new Error('stop here');
  await assert.rejects(
    readNdjson(body('{"n":1}\n{"n":2}\n'), () => {
      throw boom;
    }),
    (e) => e === boom,
  );
});
