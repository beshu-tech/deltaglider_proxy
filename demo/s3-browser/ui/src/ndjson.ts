/**
 * A reader for newline-delimited JSON (NDJSON) response bodies: one JSON
 * value per line, read while the body still arrives. The bulk delete
 * streams its progress this way (adminApi/bulkObjects.ts).
 *
 * Fetch-free (the body is passed in) so the node unit project tests it —
 * see src/__tests__/ndjson.test.ts.
 */

/** The media type of an NDJSON body. */
export const NDJSON = 'application/x-ndjson';

/**
 * Read `body` line by line and give each parsed line to `onLine`, as soon
 * as the line is whole. A line may arrive in several chunks, and a chunk
 * may hold several lines. The last line needs no newline; blank lines are
 * skipped. A line that is not JSON, or an error that `onLine` throws,
 * cancels the body and rejects.
 */
export async function readNdjson(body: ReadableStream<Uint8Array>, onLine: (value: unknown) => void): Promise<void> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let pending = '';
  const take = (raw: string) => {
    const text = raw.trim();
    if (!text) return;
    let value: unknown;
    try {
      value = JSON.parse(text);
    } catch {
      throw new Error(`Malformed line in the answer: ${text.slice(0, 120)}`);
    }
    onLine(value);
  };
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      pending += decoder.decode(value, { stream: true });
      for (let nl = pending.indexOf('\n'); nl >= 0; nl = pending.indexOf('\n')) {
        const line = pending.slice(0, nl);
        pending = pending.slice(nl + 1);
        take(line);
      }
    }
    take(pending + decoder.decode());
  } catch (e) {
    await reader.cancel().catch(() => undefined);
    throw e;
  } finally {
    reader.releaseLock();
  }
}
