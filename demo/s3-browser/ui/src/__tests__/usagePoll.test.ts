// Regression guard for COMPUTE-SIZE-POLL-FOREVER: the folder-size poll was a
// setInterval(async) that swallowed every error, so an expired session or a
// 403 polled every 2s forever (and slow responses overlapped). usagePollStep
// decides after each poll: done / retry / fail — and retries are bounded.
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { usagePollStep, type UsagePollStep } from '../usagePoll';
import { ApiError } from '../errorHandling';
import type { PrefixUsageLookup } from '../adminApi';

const SCANNING: PrefixUsageLookup = { state: 'scanning' };
const READY = { state: 'ready', entry: { total_size: 1 } } as unknown as PrefixUsageLookup;

// A failed scan caches no result. The server once answered it as "not
// cached, not scanning", which the poll read as "not yet" for 5 minutes and
// then said "Timed out". The server now keeps the failure, and the poll stops.
test('a failed scan stops the poll with its error; a scan that left no answer stops too', () => {
  assert.deepEqual(usagePollStep(1, { result: { state: 'failed', error: 'listing b failed: SlowDown' } }), {
    kind: 'fail',
    error: 'The size scan failed: listing b failed: SlowDown',
  });
  assert.deepEqual(usagePollStep(1, { result: { state: 'none' } }), {
    kind: 'fail',
    error: 'The size scan ended without a result. Try again.',
  });
});

// The budget is module-private; find it by probing, and pin its range.
function findMaxAttempts(): number {
  let attempts = 1;
  while (usagePollStep(attempts, { result: SCANNING }).kind === 'retry') {
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
  assert.equal(kind(usagePollStep(1, { result: READY })), 'done');
  assert.equal(kind(usagePollStep(1, { result: SCANNING })), 'retry');
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
  assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS - 1, { result: SCANNING })), 'retry');
  assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { result: SCANNING })), 'fail');
  assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { error: new ApiError('boom', 503) })), 'fail');
  // ...but a result on the last attempt still wins
  assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { result: READY })), 'done');
});
