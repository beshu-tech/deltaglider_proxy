// Regression guard for COMPUTE-SIZE-POLL-FOREVER: the folder-size poll was a
// setInterval(async) that swallowed every error, so an expired session or a
// 403 polled every 2s forever (and slow responses overlapped). usagePollStep
// decides after each poll: done / retry / fail — and retries are bounded.
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { usagePollStep, type UsagePollStep } from '../usagePoll';
import { ApiError } from '../errorHandling';

// The budget is module-private; find it by probing, and pin its range.
function findMaxAttempts(): number {
  let attempts = 1;
  while (usagePollStep(attempts, { result: null }).kind === 'retry') {
    attempts += 1;
    assert.ok(attempts < 10_000, 'the poll must be bounded');
  }
  return attempts;
}

const USAGE_POLL_MAX_ATTEMPTS = findMaxAttempts();

test('the probed attempt budget covers at least ~1 minute at 2s', () => {
  assert.ok(USAGE_POLL_MAX_ATTEMPTS >= 30, 'budget covers at least ~1 minute at 2s');
});

const kind = (s: UsagePollStep): UsagePollStep['kind'] => s.kind;

test('result → done; not-yet-cached → retry', () => {
  assert.equal(kind(usagePollStep(1, { result: { total_size: 1 } })), 'done');
  assert.equal(kind(usagePollStep(1, { result: null })), 'retry');
});

test('transient errors retry', () => {
  assert.equal(kind(usagePollStep(1, { error: new TypeError('Failed to fetch') })), 'retry');
  assert.equal(kind(usagePollStep(1, { error: new ApiError('boom', 502) })), 'retry');
  assert.equal(kind(usagePollStep(1, { error: new ApiError('slow', 429) })), 'retry');
  assert.equal(kind(usagePollStep(1, { error: new ApiError('slow', 408) })), 'retry');
});

test('non-retryable errors stop at once', () => {
  const expired = usagePollStep(1, { error: new ApiError('Unauthorized', 401) });
  assert.deepEqual(expired, { kind: 'fail', error: 'Session expired — sign in again' });
  assert.equal(
    kind(usagePollStep(1, { error: new ApiError('Forbidden', 403, undefined, 'admin_session_required') })),
    'fail',
  );
  assert.deepEqual(usagePollStep(1, { error: new ApiError('Bad bucket', 400) }), { kind: 'fail', error: 'Bad bucket' });
});

test('the attempt budget bounds both "no result yet" and transient errors', () => {
  assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS - 1, { result: null })), 'retry');
  assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { result: null })), 'fail');
  assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { error: new ApiError('boom', 503) })), 'fail');
  // ...but a result on the last attempt still wins
  assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { result: { total_size: 1 } })), 'done');
});
