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
