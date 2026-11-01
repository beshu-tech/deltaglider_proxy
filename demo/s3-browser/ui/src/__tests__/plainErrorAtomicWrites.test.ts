/**
 * Review finding 7: a refusal for lack of atomic writes (a read-only
 * encrypted bucket: 403 AccessDenied; a backend not checked yet or not
 * verified: 503 SlowDown) keeps the server's sentence. The fixed
 * "You do not have permission" sentence sent a user to an administrator
 * for a permission that they already have.
 */
import { describe, expect, it } from 'vitest';
import { normalizeS3Error } from '../errorHandling';
import { plainError } from '../plainError';
import { CAPABILITY_DOC_URL } from '../linkifyDocUrl';

const READ_ONLY = `Bucket 'releases' is read-only: it is encrypted, and its backend does not support atomic writes, which encryption needs. Reads keep working. To write to it again, move the bucket to a backend with atomic writes with a migrate job — see ${CAPABILITY_DOC_URL}`;
const PENDING = `Bucket 'releases' is encrypted, and the proxy has not verified yet that its backend supports atomic writes, which encryption needs. Writes wait until it has; retry in a few seconds. Reads keep working — see ${CAPABILITY_DOC_URL}`;

function sdkError(code: string, status: number, message: string) {
  return Object.assign(new Error(message), { name: code, Code: code, $metadata: { httpStatusCode: status, requestId: 'r1' } });
}

describe('atomic-writes refusals keep the server sentence', () => {
  it('a read-only 403 names the reason, not a missing permission', () => {
    const p = plainError(normalizeS3Error(sdkError('AccessDenied', 403, READ_ONLY), 'Upload object a.zip'), 'The upload failed.');
    expect(p.text).toContain('is read-only');
    expect(p.text).toContain(CAPABILITY_DOC_URL);
    expect(p.text).not.toMatch(/permission/);
  });

  it('the details keep the whole server text (not cut at 280 characters)', () => {
    const p = plainError(normalizeS3Error(sdkError('AccessDenied', 403, READ_ONLY), 'Upload object a.zip'), 'The upload failed.');
    expect(p.details).toContain(CAPABILITY_DOC_URL);
    expect(p.details).not.toContain('…');
  });

  it('a pending 503 says to retry, with the reason', () => {
    const p = plainError(normalizeS3Error(sdkError('SlowDown', 503, PENDING), 'Upload object a.zip'), 'The upload failed.');
    expect(p.text).toContain('retry in a few seconds');
  });

  it('a plain AccessDenied keeps the permission sentence', () => {
    const p = plainError(normalizeS3Error(sdkError('AccessDenied', 403, 'Access Denied'), 'Upload object a.zip'), 'The upload failed.');
    expect(p.text).toBe('You do not have permission to do this. Ask an administrator for access.');
  });
});
