/**
 * Pure decision for the folder-size ("Compute Size") result poll in
 * `useComputeSize.ts`: after each poll, continue, finish, or stop with an
 * error. Kept React-free so a Node regression script can check the table
 * (scripts/usage-poll-regression-test.mjs).
 */
import { ApiError, isSessionExpired, normalizeUiError } from './errorHandling';

/** Delay between the end of one poll and the start of the next. */
export const USAGE_POLL_INTERVAL_MS = 2000;
/** Give up after this many polls (~5 minutes at the interval above). */
const USAGE_POLL_MAX_ATTEMPTS = 150;

export type UsagePollStep = { kind: 'done' } | { kind: 'retry' } | { kind: 'fail'; error: string };

/**
 * `attempt` is 1-based (the poll that just finished). A result finishes; a
 * missing result or a transient error (network, 5xx, 408, 429) retries until
 * the attempt budget runs out; an expired session or any other 4xx stops at
 * once, because polling again cannot succeed.
 */
export function usagePollStep(
  attempt: number,
  outcome: { result: unknown } | { error: unknown },
): UsagePollStep {
  if ('error' in outcome) {
    const err = outcome.error;
    if (isSessionExpired(err)) return { kind: 'fail', error: 'Session expired — sign in again' };
    if (err instanceof ApiError && err.status >= 400 && err.status < 500 && err.status !== 408 && err.status !== 429) {
      return { kind: 'fail', error: normalizeUiError(err, 'Compute size failed') };
    }
  } else if (outcome.result) {
    return { kind: 'done' };
  }
  if (attempt >= USAGE_POLL_MAX_ATTEMPTS) {
    return { kind: 'fail', error: 'Timed out waiting for the size scan' };
  }
  return { kind: 'retry' };
}
