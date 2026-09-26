import assert from 'node:assert/strict';
import { test } from 'vitest';
import { requestIdSuffix } from '../errorHandling';

test('requestIdSuffix: empty / nullish → no suffix', () => {
  // Empty / nullish → no suffix (the 6 error-construction paths all relied on
  // the `requestId ? ... : ''` ternary; this helper IS that ternary).
  assert.equal(requestIdSuffix(''), '');
  assert.equal(requestIdSuffix(undefined), '');
  assert.equal(requestIdSuffix(null), '');
});

test('requestIdSuffix: present → " (request-id: …)" with exact leading space and parens', () => {
  assert.equal(requestIdSuffix('abc123'), ' (request-id: abc123)');
  assert.equal(requestIdSuffix('REQ-7F3A-2025'), ' (request-id: REQ-7F3A-2025)');
});

test('requestIdSuffix: concatenation contract, appended verbatim, no extra space', () => {
  assert.equal(`oops${requestIdSuffix('x')}`, 'oops (request-id: x)');
  assert.equal(`oops${requestIdSuffix('')}`, 'oops');
});
