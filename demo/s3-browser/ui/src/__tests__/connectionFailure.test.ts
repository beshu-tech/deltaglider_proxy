/** src/s3client.ts connectionFailure: the sign-in card's words (B5). */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { connectionFailure } from '../s3client';

const s3err = (code: string, status: number, message = code) =>
  Object.assign(new Error(message), { name: code, Code: code, $metadata: { httpStatusCode: status, requestId: '4657c220-aaaa' } });

test('wrong key or secret: one plain sentence, the request id kept apart', () => {
  for (const code of ['SignatureDoesNotMatch', 'InvalidAccessKeyId', 'AccessDenied']) {
    assert.deepEqual(connectionFailure(s3err(code, 403)), {
      error: 'The access key ID or the secret access key is wrong.',
      requestId: '4657c220-aaaa',
    });
  }
});

test('no answer from the server says so', () => {
  assert.equal(
    connectionFailure(new TypeError('Failed to fetch')).error,
    'Could not reach the server. Check the network connection, then try again.',
  );
});

test('other failures keep the server message, without the request id', () => {
  const r = connectionFailure(s3err('SlowDown', 503, 'Backend unhealthy'));
  assert.match(r.error, /Backend unhealthy/);
  assert.doesNotMatch(r.error, /request-id/);
  assert.equal(r.requestId, '4657c220-aaaa');
});
