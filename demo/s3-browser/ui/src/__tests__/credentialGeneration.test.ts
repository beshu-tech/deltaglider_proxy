import assert from 'node:assert/strict';
import { test } from 'vitest';
import { generateId, generateSecret } from '../credentialGeneration';

// Deterministic fill: ramp 0,1,2,... so output is reproducible. Lets us assert
// the exact alphabet mapping (b % alphabet.length).
const ramp = (buf: Uint8Array): void => {
  for (let i = 0; i < buf.length; i++) buf[i] = i;
};
// Constant fill: every byte 0 -> every body char is alphabet[0].
const zeros = (buf: Uint8Array): void => {
  buf.fill(0);
};

const ID_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';

test('generateId: AK prefix, 18-char uppercase-alnum body, exact alphabet mapping', () => {
  const id = generateId(ramp);
  assert.ok(id.startsWith('AK'), 'id must start with AK');
  assert.equal(id.length, 2 + 18, 'AK + 18 body chars');
  const body = id.slice(2);
  assert.match(body, /^[A-Z0-9]+$/, 'body is uppercase-alnum only');
  // ramp body: bytes 0..17 -> ID_ALPHABET[0..17]
  assert.equal(body, ID_ALPHABET.slice(0, 18));

  // zeros -> every body char is the first alphabet char ('A').
  assert.equal(generateId(zeros), 'AK' + 'A'.repeat(18));
});

test('generateSecret: 40-char base64-alphabet secret', () => {
  const secret = generateSecret(ramp);
  assert.equal(secret.length, 40, 'secret is 40 chars');
  // base64 alphabet (A-Za-z0-9+/), no padding.
  assert.match(secret, /^[A-Za-z0-9+/]+$/);
  assert.equal(generateSecret(zeros).length, 40);
});

test('default CSPRNG path (smoke)', () => {
  const a = generateId();
  const b = generateId();
  assert.notEqual(a, b, 'two CSPRNG ids should differ');
  assert.ok(a.startsWith('AK') && a.length === 20);
  assert.equal(generateSecret().length, 40);
});
