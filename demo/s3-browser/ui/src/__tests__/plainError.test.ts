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
