/**
 * Pure helpers of the Verify tab (the replication parity audit): phase
 * words, the progress line, the decaying rate, the advice for objects the
 * audit could not read, and the reason the audit buttons are off. Kept out
 * of the component so each rule is unit-tested (verifyView.test.ts).
 */
import type { ParityOutcome, ParityPhase, UnresolvedKey } from '../../adminApi';

/** The words of the running audit's phase (the headline of the wait panel). */
export function verifyPhaseLabel(
  phase: ParityPhase | undefined,
  opts: { cancelling?: boolean; rechecking?: boolean } = {},
): string {
  if (opts.cancelling) return 'Cancelling…';
  if (opts.rechecking) return 'Re-checking the objects that could not be read…';
  switch (phase) {
    case 'listing_source':
      return 'Listing the source…';
    case 'listing_destination':
      return 'Listing the destination…';
    case 'reading':
      return 'Reading object metadata…';
    case 'comparing':
      return 'Comparing source and destination…';
    default:
      return 'Starting the audit…';
  }
}

/**
 * The count line and bar of the running audit, in OBJECTS. A listing phase
 * counts against the previous audit's count of that side, an estimate: it
 * reads "about", and past the estimate the bar goes indeterminate rather
 * than sit at 100 % while the listing goes on.
 */
export function verifyProgress(
  scanned: number | undefined,
  total: number | undefined,
  phase: ParityPhase | undefined,
): { text: string; determinate: boolean; percent: number } {
  const n = Math.max(0, scanned ?? 0);
  const t = total ?? 0;
  const listing = phase === 'listing_source' || phase === 'listing_destination';
  if (phase === 'comparing') return { text: '', determinate: false, percent: 0 };
  if (t > 0 && n <= t) {
    const of = listing ? `of about ${t.toLocaleString()}` : `of ${t.toLocaleString()}`;
    const what = phase === 'reading' ? 'objects read' : listing ? 'objects listed' : 'objects';
    return {
      text: `${n.toLocaleString()} ${of} ${what}`,
      determinate: true,
      percent: Math.max(0, Math.min(100, Math.round((n / t) * 100))),
    };
  }
  const what = phase === 'reading' ? 'read' : 'listed';
  return {
    text: `${n.toLocaleString()} ${n === 1 ? 'object' : 'objects'} ${what}`,
    determinate: false,
    percent: 0,
  };
}

/** Seconds without forward progress before the rate starts to decay. The
 *  server writes the count in chunks, so a short gap is normal. */
const RATE_GRACE_S = 6;
/** Seconds without forward progress before the panel says "no progress". */
const STALL_S = 30;

/**
 * The rate to show, given the smoothed rate at the last forward sample and
 * how long ago that sample was. Inside the grace period: the rate as it was.
 * After it: the rate decays as grace/elapsed, so a stalled audit no longer
 * shows the speed it had a minute ago. Past STALL_S: "no progress for N s".
 */
export function displayRate(
  rate: number | null,
  lastForwardMs: number | null,
  nowMs: number,
): string {
  if (rate == null || rate <= 0 || lastForwardMs == null) return '—/s';
  const idle = Math.max(0, (nowMs - lastForwardMs) / 1000);
  if (idle >= STALL_S) return `no progress for ${Math.round(idle)} s`;
  const shown = idle <= RATE_GRACE_S ? rate : rate * (RATE_GRACE_S / idle);
  if (shown < 0.05) return '—/s';
  return `~${shown < 10 ? shown.toFixed(1) : Math.round(shown).toLocaleString()}/s`;
}

/** What made the reads fail, from their error texts. */
export type UnreadCause = 'throttled' | 'timeout' | 'unknown';

/**
 * The cause that the kept errors point at. Throttling (503 SlowDown, 429)
 * is the one cause that fewer parallel reads fix; a timeout means the
 * backend did not answer in time, and lowering the concurrency does not
 * help that.
 */
export function unreadCause(keys: UnresolvedKey[] | undefined): UnreadCause {
  const errors = (keys ?? []).map((k) => (k.error ?? '').toLowerCase()).filter(Boolean);
  if (errors.length === 0) return 'unknown';
  const throttled = errors.filter((e) => /slow ?down|throttl|too many requests|\b429\b|\b503\b/.test(e));
  const timedOut = errors.filter((e) => /timed? ?out|timeout|deadline/.test(e));
  if (throttled.length >= timedOut.length && throttled.length > 0) return 'throttled';
  if (timedOut.length > 0) return 'timeout';
  return 'unknown';
}

/** The distinct error texts of the kept keys, most frequent first. */
export function distinctErrors(keys: UnresolvedKey[] | undefined, max = 3): string[] {
  const counts = new Map<string, number>();
  for (const k of keys ?? []) {
    if (k.error) counts.set(k.error, (counts.get(k.error) ?? 0) + 1);
  }
  return [...counts.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, max)
    .map(([e]) => e);
}

/**
 * Why the audit buttons are off, or null when an audit may start. The server
 * refuses an audit while a run of the rule is in progress (the destination is
 * mid-sync), and the button said nothing until the click failed with a raw 409.
 */
export function auditBlockedReason(ruleStatus: string | undefined): string | null {
  if (ruleStatus === 'running' || ruleStatus === 'cancelling' || ruleStatus === 'queued') {
    return 'A run of this rule is in progress. Audit after it ends: a mirror that is still copying cannot be in sync.';
  }
  return null;
}

/** Short headline word from the authoritative verdict (+ truncation nuance).
 *  Legacy fallback mirrors toneFor/legacyBody: ANY real diff (missing/extra/
 *  mismatch) → at_risk, so the headline can't read "not fully verified" while
 *  the halo + body say files are missing. */
export function headlineFor(o: ParityOutcome): string {
  const realDiffs = o.missing_on_dest + o.orphan_on_dest + o.checksum_mismatch;
  const v = o.verdict ?? (o.in_sync ? 'safe' : realDiffs > 0 ? 'at_risk' : 'incomplete');
  // "Scanned portion" only when the listing stopped at the cap. Objects that
  // could not be read leave the whole mirror listed; the body names them.
  const listingCut = o.cap_hit ?? (o.truncated && (o.unresolved ?? 0) === 0);
  const scoped = listingCut ? ' (scanned portion)' : '';
  switch (v) {
    case 'safe':
      return `Verified in sync${scoped}`;
    case 'incomplete':
      return `Not fully verified${scoped}`;
    case 'at_risk':
      return `Differences found${scoped}`;
  }
}
