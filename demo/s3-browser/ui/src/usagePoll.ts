/**
 * Pure decision for the folder-size ("Compute Size") result poll in
 * `useComputeSize.ts`: after each poll, continue, finish, or stop with an
 * error. Kept React-free so a unit test can check the table
 * (src/__tests__/usagePoll.test.ts).
 */
import { ApiError, isSessionExpired, normalizeUiError } from './errorHandling';
import type { PrefixUsageLookup } from './adminApi';

/** Delay between the end of one poll and the start of the next. */
export const USAGE_POLL_INTERVAL_MS = 2000;
/** Give up after this many polls (~5 minutes at the interval above). */
const USAGE_POLL_MAX_ATTEMPTS = 150;

export type UsagePollStep = { kind: 'done' } | { kind: 'retry' } | { kind: 'fail'; error: string };

/**
 * `attempt` is 1-based (the poll that just finished), after the scan was
 * asked for. A result finishes. A failed scan stops at once with its error:
 * the server caches no result for it, so polling again only waited for the
 * timeout. "No result, no scan, no failure" also stops: the scan ended and
 * its answer is gone (a tab hidden for longer than the server keeps a
 * failure). A running scan or a transient error (network, 5xx, 408, 429)
 * retries until the attempt budget runs out; an expired session or any
 * other 4xx stops at once, because polling again cannot succeed.
 */
export function usagePollStep(
  attempt: number,
  outcome: { result: PrefixUsageLookup } | { error: unknown },
): UsagePollStep {
  if ('error' in outcome) {
    const err = outcome.error;
    if (isSessionExpired(err)) return { kind: 'fail', error: 'Session expired — sign in again' };
    if (err instanceof ApiError && err.status >= 400 && err.status < 500 && err.status !== 408 && err.status !== 429) {
      return { kind: 'fail', error: normalizeUiError(err, 'Compute size failed') };
    }
  } else {
    const r = outcome.result;
    if (r.state === 'ready') return { kind: 'done' };
    if (r.state === 'failed') return { kind: 'fail', error: `The size scan failed: ${r.error}` };
    if (r.state === 'none') return { kind: 'fail', error: 'The size scan ended without a result. Try again.' };
  }
  if (attempt >= USAGE_POLL_MAX_ATTEMPTS) {
    return { kind: 'fail', error: 'Timed out waiting for the size scan' };
  }
  return { kind: 'retry' };
}
