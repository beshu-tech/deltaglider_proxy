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
 * Summary chip for an event's per-endpoint delivery state: how many endpoints
 * accepted it. `null` until an endpoint is attempted. All accepted is green,
 * a partial result (the row retries the rest) amber, none accepted red.
 */
export function endpointDeliverySummary(
  deliveries: { status: string }[],
): { text: string; tone: Tone } | null {
  if (deliveries.length === 0) return null;
  const ok = deliveries.filter((d) => d.status === 'delivered').length;
  const n = deliveries.length;
  const tone: Tone = ok === n ? 'success' : ok === 0 ? 'error' : 'warning';
  return { text: `${ok}/${n} endpoint${n === 1 ? '' : 's'}`, tone };
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

/** Fewest cache lookups before the hit rate may turn amber or red. */
const MIN_CACHE_SAMPLE = 20;

/**
 * Severity of the dashboard cache hit rate. Right after a start every lookup
 * misses (1 miss of 1 lookup is a 100 % miss rate), so the card stays
 * neutral until there are enough lookups to mean something.
 */
export function cacheMissSeverity(missRate: number, lookups: number): 'neutral' | 'good' | 'warn' | 'bad' {
  if (lookups < MIN_CACHE_SAMPLE) return 'neutral';
  if (missRate > 0.5) return 'bad';
  if (missRate > 0.2) return 'warn';
  return 'good';
}
