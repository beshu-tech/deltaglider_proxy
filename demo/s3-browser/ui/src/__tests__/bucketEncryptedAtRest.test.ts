/** The padlock's hover text comes from the server's `effective_encryption`. */
import { expect, test } from 'vitest';
import { encryptedAtRestText } from '../bucketEncryption';
import { eff } from '../test/encryption';

test('plaintext, a refused proxy key, or no answer: no padlock', () => {
  expect(encryptedAtRestText(undefined)).toBeNull();
  expect(encryptedAtRestText(null)).toBeNull();
  expect(encryptedAtRestText(eff())).toBeNull();
  expect(encryptedAtRestText(eff({ mode: 'aes256-gcm-proxy', has_key: false, encrypts: false }))).toBeNull();
});

test('the text names the mode, the key and where it is set', () => {
  expect(
    encryptedAtRestText(eff({ mode: 'aes256-gcm-proxy', source: 'bucket', has_key: true, encrypts: true, key_id: 'k1' })),
  ).toBe('Encrypted at rest: proxy AES-256-GCM, key k1 (set on this bucket)');
  expect(encryptedAtRestText(eff({ mode: 'sse-kms', encrypts: true, kms_key_id: 'arn:k' }))).toBe(
    'Encrypted at rest: SSE-KMS, key arn:k (set on backend hetzner-fsn1)',
  );
  expect(encryptedAtRestText(eff({ mode: 'sse-s3', encrypts: true }))).toBe(
    'Encrypted at rest: SSE-S3 (set on backend hetzner-fsn1)',
  );
});
