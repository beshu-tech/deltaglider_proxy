/**
 * The duration fields of the rule editors (Expire after, Interval, the
 * tick of a rules page) take what the server parses with `humantime`:
 * one or more `<number><unit>` parts, such as `30d`, `12h`, `1h 30m`.
 * The editors check a value as the user leaves the field, so a typo shows
 * under the field, not later as a parser message in the review dialog.
 */
import type { LifecycleRuleConfig, ReplicationRuleConfig } from '../../adminApi';

/** The unit words of `humantime::parse_duration` (humantime 2.3). */
const UNITS = [
  'nanos', 'nsec', 'ns', 'usec', 'us', 'µs', 'millis', 'msec', 'ms',
  'seconds', 'second', 'secs', 'sec', 's',
  'minutes', 'minute', 'mins', 'min', 'm',
  'hours', 'hour', 'hrs', 'hr', 'h',
  'days', 'day', 'd',
  'weeks', 'week', 'wks', 'wk', 'w',
  'months', 'month', 'M',
  'years', 'year', 'yrs', 'yr', 'y',
];
const PART = `\\d+(?:\\.\\d+)?\\s*(?:${UNITS.join('|')})`;
const DURATION = new RegExp(`^\\s*${PART}(?:\\s*${PART})*\\s*$`);

const SECONDS: Record<string, number> = {
  s: 1, sec: 1, secs: 1, second: 1, seconds: 1,
  m: 60, min: 60, mins: 60, minute: 60, minutes: 60,
  h: 3600, hr: 3600, hrs: 3600, hour: 3600, hours: 3600,
  d: 86400, day: 86400, days: 86400,
  w: 604800, wk: 604800, wks: 604800, week: 604800, weeks: 604800,
  M: 2_630_016, month: 2_630_016, months: 2_630_016,
  y: 31_557_600, yr: 31_557_600, yrs: 31_557_600, year: 31_557_600, years: 31_557_600,
};

/** Pure: the length of a valid duration in seconds (sub-second units count
 *  as 0), or null when the server would refuse the text. */
export function durationSeconds(text: string): number | null {
  if (text.trim() === '0') return 0;
  if (!DURATION.test(text)) return null;
  let total = 0;
  for (const m of text.matchAll(/(\d+(?:\.\d+)?)\s*([A-Za-zµ]+)/g)) {
    total += Number(m[1]) * (SECONDS[m[2]] ?? 0);
  }
  return total;
}

const DURATION_HINT = 'Use a duration such as 30d, 12h or 90m.';

/**
 * Pure: the error sentence of a duration field, or null when the value is
 * good. `optional`: an empty value is good (the field has a default or
 * means "no filter"). `minSeconds`: the server's minimum, with its words.
 */
export function durationError(
  value: string | null | undefined,
  opts: { optional?: boolean; min?: { seconds: number; text: string } } = {},
): string | null {
  const text = (value ?? '').trim();
  if (!text) return opts.optional ? null : `Enter a duration. ${DURATION_HINT}`;
  const secs = durationSeconds(text);
  if (secs === null) return `"${text}" is not a duration. ${DURATION_HINT}`;
  if (opts.min && secs < opts.min.seconds) return `Enter ${opts.min.text} or more.`;
  return null;
}

type LifecycleDurationField = 'expire_after' | 'min_age' | 'protect_younger_than';

/** Pure: the duration errors of one lifecycle rule, by field. */
export function lifecycleRuleFieldErrors(rule: LifecycleRuleConfig): Partial<Record<LifecycleDurationField, string>> {
  const out: Partial<Record<LifecycleDurationField, string>> = {};
  const action = typeof rule.action === 'object' ? rule.action : null;
  if (action?.type === 'retain-newest') {
    const minAge = durationError(action.qualify?.min_age, { optional: true });
    if (minAge) out.min_age = minAge;
    const protect = durationError(action.protect_younger_than, { optional: true });
    if (protect) out.protect_younger_than = protect;
  } else {
    // 0 would expire every object at once: the server refuses it.
    const expire = durationError(rule.expire_after, { min: { seconds: 1, text: '1s' } });
    if (expire) out.expire_after = expire;
  }
  return out;
}

/** The server's minimum of a replication rule's interval (`MIN_INTERVAL`). */
const REPLICATION_MIN_INTERVAL = { seconds: 30, text: '30s' };

/** Pure: the duration errors of one replication rule, by field. */
export function replicationRuleFieldErrors(rule: ReplicationRuleConfig): Partial<Record<'interval', string>> {
  const interval = durationError(rule.interval, { optional: true, min: REPLICATION_MIN_INTERVAL });
  return interval ? { interval } : {};
}

const FIELD_WORDS: Record<string, string> = {
  expire_after: 'Expire after',
  min_age: 'Ignore objects younger than',
  protect_younger_than: "Don't delete anything younger than",
  interval: 'Interval',
};

/** Pure: the first field error of a rule list, as one sentence that names
 *  the rule and the field, or null when every duration is good. */
export function firstRuleFieldError<R extends { name: string }>(
  rules: readonly R[],
  errorsOf: (rule: R) => Partial<Record<string, string>>,
): string | null {
  for (const rule of rules) {
    const [field, error] = Object.entries(errorsOf(rule)).find(([, e]) => e) ?? [];
    if (field && error) return `Rule ${rule.name || '(unnamed)'}, ${FIELD_WORDS[field] ?? field}: ${error}`;
  }
  return null;
}
