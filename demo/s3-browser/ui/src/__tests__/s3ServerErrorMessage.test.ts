/**
 * A 5xx from the proxy is an S3 XML error with a message that says why (rate
 * limit, concurrency cap, spool budget, codec slots, an object that changed
 * while it was read, an admission reject). The browser showed it only when
 * it matched a short word list, and else a "verify gateway/proxy limits"
 * hint. Now the message shows whenever the SDK parsed an S3 XML error; the
 * hint stays for bodies that are not ours.
 */
import { expect, test } from 'vitest';
import { normalizeS3Error } from '../errorHandling';

/** An error as the AWS SDK throws it after it parsed `<Error>` XML. */
function parsed(status: number, code: string, message: string) {
  return Object.assign(new Error(message), { name: code, $metadata: { httpStatusCode: status, requestId: 'rid-1' } });
}

test.each([
  ['SlowDown', 'Too many requests at once: the proxy serves 64 requests at a time. Retry later.'],
  ['SlowDown', 'The spool budget is full. Retry later.'],
  ['SlowDown', 'The object changed while it was read. Retry the request.'],
  ['SlowDown', 'maintenance window'],
  ['ServiceUnavailable', 'All codec slots are busy.'],
])('a %s from the proxy shows its message: %s', (code, message) => {
  const text = normalizeS3Error(parsed(503, code, message), 'Upload').message;
  expect(text).toBe(`Upload failed (503) [${code}]: ${message} (request-id: rid-1)`);
  expect(text).not.toMatch(/gateway/i);
});

test('a 503 body that is not an S3 XML error keeps the gateway hint', () => {
  // Empty body: the SDK names it Unknown with the message UnknownError.
  const empty = Object.assign(new Error('UnknownError'), { name: 'Unknown', $metadata: { httpStatusCode: 503 } });
  expect(normalizeS3Error(empty, 'Upload').message).toMatch(/verify gateway\/proxy limits/);
  // An HTML page from a reverse proxy: the SDK's XML parse fails.
  const html = Object.assign(new Error('Unexpected token < in XML parsing error at char 0'), {
    name: 'Error',
    $metadata: { httpStatusCode: 503 },
  });
  expect(normalizeS3Error(html, 'Upload').message).toMatch(/verify gateway\/proxy limits/);
  // A 502 with no parsed body.
  const bad = Object.assign(new Error('UnknownError'), { name: 'Unknown', $metadata: { httpStatusCode: 502 } });
  expect(normalizeS3Error(bad, 'Upload').message).toMatch(/502 Bad Gateway/);
});
