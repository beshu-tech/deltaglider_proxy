/**
 * P3: the encryption witness refuses writes of a bucket with 503 SlowDown
 * and a message that says why. An upload error shows that message, not
 * the generic gateway hint.
 */
import { expect, test } from 'vitest';
import { normalizeS3Error } from '../errorHandling';

test('a witness refusal reaches the operator verbatim', () => {
  const message =
    "the encryption witness of this instance is not synced: witnessed buckets refuse writes until it syncs again. Reads keep working";
  const err = Object.assign(new Error(message), { name: 'SlowDown', $metadata: { httpStatusCode: 503 } });
  expect(normalizeS3Error(err, 'Upload').message).toContain('witnessed buckets refuse writes');
});
