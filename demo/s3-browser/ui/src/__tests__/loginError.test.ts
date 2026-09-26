/** src/loginError.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { loginFailureMessage, retryAfterSeconds } from '../loginError';

test('a lockout names itself and the wait, never "Login failed: Login failed"', () => {
  // Explore finding 12: the 4th wrong password (and the right one, while
  // locked) read "Login failed: Login failed".
  assert.equal(
    loginFailureMessage({ status: 429, retryAfterSecs: 540 }),
    'Too many sign-in attempts. Try again in 9 min.',
  );
  assert.equal(
    loginFailureMessage({ status: 429, retryAfterSecs: 30 }),
    'Too many sign-in attempts. Try again in 30 s.',
  );
  assert.equal(
    loginFailureMessage({ status: 429, retryAfterSecs: null }),
    'Too many sign-in attempts. Wait a few minutes, then try again.',
  );
});

test('a wrong password says so once', () => {
  assert.equal(loginFailureMessage({ status: 401, error: 'Login failed' }), 'Login failed: wrong password.');
  assert.equal(loginFailureMessage({ status: 401 }), 'Login failed: wrong password.');
  assert.equal(loginFailureMessage({ status: 500, error: 'db locked' }), 'Login failed: db locked');
});

test('Retry-After: delta seconds, HTTP date, body fallback, garbage', () => {
  const now = Date.parse('2026-09-26T10:00:00Z');
  assert.equal(retryAfterSeconds('120', undefined, now), 120);
  assert.equal(retryAfterSeconds('Sat, 26 Sep 2026 10:05:00 GMT', undefined, now), 300);
  assert.equal(retryAfterSeconds(null, { retry_after_secs: 42 }, now), 42);
  assert.equal(retryAfterSeconds('soon', undefined, now), null);
  assert.equal(retryAfterSeconds(null, undefined, now), null);
});
