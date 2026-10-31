/**
 * THE error-to-words rule (UX harvest C2): the backend create error read
 * "API /_/api/admin/backends failed (400): backend 'qa-fs-backend' failed
 * its connection probe — endpoint unreachable (/nonexistent/path/for/qa:
 * Permission denied (os error 13)) — check endpoint / network / DNS. …".
 */
import { describe, expect, test } from 'vitest';
import { backendHealthSentence, plainError } from '../adminApi';
import { ApiError } from '../errorHandling';

const probeError = (cause: string) =>
  new ApiError(
    `API /_/api/admin/backends failed (400): backend 'qa-fs-backend' failed its connection probe — ${cause}. Fix the endpoint/credentials and retry (nothing was changed)`,
    400,
    undefined,
    `backend 'qa-fs-backend' failed its connection probe — ${cause}. Fix the endpoint/credentials and retry (nothing was changed)`,
  );

describe('plainError: a failed backend probe', () => {
  test.each([
    [
      'endpoint unreachable (/nonexistent/path/for/qa: Permission denied (os error 13)) — check endpoint / network / DNS',
      'The proxy cannot write to /nonexistent/path/for/qa (permission denied). Nothing was changed.',
    ],
    [
      'endpoint unreachable (/data/x: Read-only file system (os error 30)) — check endpoint / network / DNS',
      'The proxy cannot write to /data/x: the file system is read-only. Nothing was changed.',
    ],
    [
      'endpoint unreachable (dispatch failure: dns error: failed to lookup address information) — check endpoint / network / DNS',
      'The proxy cannot find the endpoint host name (DNS). Nothing was changed.',
    ],
    [
      'endpoint unreachable (dispatch failure: Connection refused) — check endpoint / network / DNS',
      'The endpoint refuses the connection. Nothing was changed.',
    ],
    [
      'credentials rejected (InvalidAccessKeyId) — check access_key_id / secret_access_key',
      'The backend refuses the access key or the secret key. Nothing was changed.',
    ],
  ])('%s', (cause, sentence) => {
    const out = plainError(probeError(cause), 'x');
    expect(out.text).toBe(sentence);
    // The raw text stays available, for a support request.
    expect(out.details).toContain(cause);
    expect(out.text).not.toMatch(/\(400\)|os error|\/_\/api/);
  });
});

describe('plainError: other errors', () => {
  test('an ApiError with server text drops the path and the status', () => {
    const e = new ApiError('API /_/api/admin/backends failed (409): name already taken', 409, 'conflict', 'name already taken');
    expect(plainError(e, 'x')).toEqual({ text: 'Name already taken.', details: e.message });
  });
  test('an ApiError without server text falls back', () => {
    const e = new ApiError('API /_/api/admin/backends failed (500): Server internal error.', 500);
    expect(plainError(e, 'The proxy did not create the backend.').text).toBe('The proxy did not create the backend.');
  });
  test('an identifier keeps its case', () => {
    expect(plainError('cache_size_mb must be > 0', 'x').text).toBe('cache_size_mb must be > 0.');
  });
  test('a bad duration in a config error', () => {
    expect(plainError("lifecycle rule 'qa-lc-rule' expire_after=notaduration invalid: expected number at 0", 'x').text).toBe(
      '"notaduration" is not a valid duration for expire after of rule qa-lc-rule. Use a duration such as 30d, 12h or 90m.',
    );
  });
});

test('backendHealthSentence', () => {
  expect(backendHealthSentence('unreachable', '/srv/dg: No such file or directory (os error 2)')).toBe(
    'The proxy cannot use /srv/dg: no such file or directory.',
  );
  expect(backendHealthSentence('erroring', '503 SlowDown')).toBe('The backend answers, but with server errors.');
});

/** UX harvest D17, D24: S3 and HTTP failures read as one sentence; the
 *  status, the error code and the request id stay in `details`. */
describe('plainError: S3 requests', () => {
  const s3 = (status: number, code: string, message: string) =>
    Object.assign(new Error(message), { name: code, $metadata: { httpStatusCode: status, requestId: 'req-1' } });

  test('a full bucket, from the operator-facing string of describeS3Error', () => {
    const raw =
      'Upload object qa-60k-b.bin failed (403) [AccessDenied]: Bucket quota exceeded: 59 KiB used + 59 KiB upload > 100 KiB limit (request-id: 49e72a4e)';
    const out = plainError(raw, 'x');
    expect(out.text).toBe(
      'This bucket is full: 59 KiB is used of 100 KiB, and this file needs 59 KiB. Delete objects or ask an administrator to raise the quota.',
    );
    expect(out.details).toBe(raw);
  });
  test('a full bucket, from an ApiError of normalizeS3Error', () => {
    const e = new ApiError('Upload failed (403) [AccessDenied]: x (request-id: r)', 403, 'AccessDenied', 'Bucket quota exceeded: 1 KiB used + 2 KiB upload > 2 KiB limit');
    expect(plainError(e, 'x').text).toMatch(/^This bucket is full: 1 KiB is used of 2 KiB/);
  });
  test.each([
    [s3(403, 'AccessDenied', 'Access Denied.'), 'You do not have permission to do this. Ask an administrator for access.'],
    [s3(403, 'SignatureDoesNotMatch', 'sig'), 'The access key or the secret key is wrong. Sign in again with valid credentials.'],
    [s3(413, 'EntityTooLarge', 'too big'), 'The file is larger than the proxy accepts. Upload a smaller file, or ask an administrator to raise the size limit.'],
    [s3(503, 'SlowDown', 'Please reduce your request rate.'), 'The proxy is busy. Wait a moment and try again.'],
    [s3(404, 'NoSuchBucket', 'The specified bucket does not exist'), 'The bucket does not exist.'],
  ])('%s', (err, sentence) => {
    const out = plainError(err, 'x');
    expect(out.text).toBe(sentence);
    expect(out.details).toMatch(/req-1/);
  });
  test("the proxy's own SlowDown reason is kept", () => {
    const out = plainError(s3(503, 'SlowDown', 'bucket releases is busy: a migrate job runs'), 'x');
    expect(out.text).toBe('Bucket releases is busy: a migrate job runs.');
  });
  test('no network', () => {
    expect(plainError(new TypeError('Failed to fetch'), 'x').text).toBe(
      'The proxy does not answer. Check the network connection and try again.',
    );
  });
  test('an internal server error shows the fallback, never "boom"', () => {
    const e = new ApiError('Section fetch (storage) failed (500) [internal]: boom', 500, 'internal', 'boom');
    const out = plainError(e, 'The proxy could not load the bucket settings.');
    expect(out.text).toBe('The proxy could not load the bucket settings.');
    expect(out.details).toContain('boom');
  });
  test('a wrapped string with a status and code never shows them in the sentence', () => {
    const out = plainError('Failed to load: Runs failed (500) [internal]: boom', 'The proxy could not load the runs.');
    expect(out.text).toBe('The proxy could not load the runs.');
    expect(out.text).not.toMatch(/500|internal|boom/);
  });
});

test('a bare "HTTP 500" shows the fallback', () => {
  expect(plainError(new Error('HTTP 500'), 'The proxy could not load the metrics.')).toEqual({
    text: 'The proxy could not load the metrics.',
    details: 'HTTP 500',
  });
});
