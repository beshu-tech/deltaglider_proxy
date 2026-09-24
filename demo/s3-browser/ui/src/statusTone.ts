/**
 * The colour rule for status chips and counters in the admin UI:
 * red and amber mean "a real problem needs your attention". A counter of
 * zero, a normal state, or a role name is never red or amber.
 */

/** AntD Tag colour names used by the status chips. */
export type Tone = 'default' | 'success' | 'processing' | 'warning' | 'error';

/**
 * Tone for a counter chip. A zero count is neutral whatever its category:
 * "0 failed" is good news and must not look like an alarm.
 */
export function countTone(value: number, tone: Tone): Tone {
  return value > 0 ? tone : 'default';
}

/**
 * Tone for one event-log row. `pending` is the normal state of a new event,
 * so it is neutral until a delivery attempt fails (then it is retrying: amber).
 */
export function eventStatusTone(status: string, attempts: number): Tone {
  switch (status) {
    case 'delivered':
      return 'success';
    case 'failed':
      return 'error';
    case 'in_progress':
      return 'processing';
    default:
      return attempts > 0 ? 'warning' : 'default';
  }
}

/**
 * Severity of the dashboard error rate. Only server errors (5xx) count:
 * S3 clients answer many requests with a 4xx as a normal step (a HEAD before
 * a PUT answers 404), so client errors alone never turn the card amber or red.
 */
export function serverErrorSeverity(count5xx: number, total: number): 'good' | 'warn' | 'bad' {
  if (total <= 0) return 'good';
  const share = count5xx / total;
  if (share > 0.05) return 'bad';
  if (share > 0.01) return 'warn';
  return 'good';
}
