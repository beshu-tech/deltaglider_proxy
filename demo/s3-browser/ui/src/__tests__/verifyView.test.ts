/**
 * The pure rules of the Verify tab: progress in objects per phase, the rate
 * that decays during a stall, the advice for unread objects, the headline,
 * and why the audit buttons are off.
 */
import { expect, test } from 'vitest';
import type { ParityOutcome } from '../adminApi';
import {
  auditBlockedReason,
  displayRate,
  distinctErrors,
  headlineFor,
  unreadCause,
  verifyPhaseLabel,
  verifyProgress,
} from '../components/jobs/verifyView';

test('each phase has its own words', () => {
  expect(verifyPhaseLabel('listing_source')).toBe('Listing the source…');
  expect(verifyPhaseLabel('listing_destination')).toBe('Listing the destination…');
  expect(verifyPhaseLabel('reading')).toBe('Reading object metadata…');
  expect(verifyPhaseLabel('comparing')).toBe('Comparing source and destination…');
  expect(verifyPhaseLabel(undefined)).toBe('Starting the audit…');
  expect(verifyPhaseLabel('reading', { rechecking: true })).toMatch(/Re-checking/);
  expect(verifyPhaseLabel('reading', { cancelling: true })).toBe('Cancelling…');
});

test('progress counts objects, and a listing estimate reads "about"', () => {
  expect(verifyProgress(1200, 4000, 'listing_source')).toEqual({
    text: '1,200 of about 4,000 objects listed',
    determinate: true,
    percent: 30,
  });
  expect(verifyProgress(4000, 4000, 'reading')).toEqual({
    text: '4,000 of 4,000 objects read',
    determinate: true,
    percent: 100,
  });
  // Past the estimate the listing goes on: no bar stuck at 100 %.
  expect(verifyProgress(4500, 4000, 'listing_destination')).toEqual({
    text: '4,500 objects listed',
    determinate: false,
    percent: 0,
  });
  // First audit: no estimate.
  expect(verifyProgress(1, 0, 'listing_source').text).toBe('1 object listed');
  expect(verifyProgress(10, 50, 'comparing').text).toBe('');
});

test('the rate holds through a short gap, then decays, then says the audit stands still', () => {
  const t0 = 1_000_000;
  expect(displayRate(null, t0, t0)).toBe('—/s');
  expect(displayRate(120, t0, t0 + 2_000)).toBe('~120/s');
  expect(displayRate(120, t0, t0 + 6_000)).toBe('~120/s');
  // 12 s after the last forward sample: half the speed.
  expect(displayRate(120, t0, t0 + 12_000)).toBe('~60/s');
  expect(displayRate(4, t0, t0 + 24_000)).toBe('~1.0/s');
  expect(displayRate(120, t0, t0 + 45_000)).toBe('no progress for 45 s');
});

test('the advice follows the cause that the read errors name', () => {
  const k = (error?: string) => ({ dest_key: 'a', error });
  expect(unreadCause([k('source: service error: 503 SlowDown')])).toBe('throttled');
  expect(unreadCause([k('destination: operation timed out')])).toBe('timeout');
  expect(unreadCause([k('source: request timeout'), k('source: timed out'), k('destination: 429 Too Many Requests')])).toBe('timeout');
  expect(unreadCause([k('source: access denied')])).toBe('unknown');
  expect(unreadCause([k()])).toBe('unknown');
  expect(unreadCause(undefined)).toBe('unknown');
  expect(distinctErrors([k('x'), k('y'), k('y'), k()])).toEqual(['y', 'x']);
});

const base: ParityOutcome = {
  rule_name: 'r', source_bucket: 's', dest_bucket: 'd', source_objects: 3, dest_objects: 3,
  matched: 3, missing_on_dest: 0, orphan_on_dest: 0, checksum_mismatch: 0, unverifiable: 0,
  truncated: false, in_sync: true, scanned_at: 1, conflict_policy: 'content-diff',
  replicate_deletes: false,
  actionable: { rerun_fixes: 0, rerun_conditional: 0, needs_manual: 0, copy_failing: 0 },
  missing_samples: [], orphan_samples: [], mismatch_samples: [], verdict: 'safe',
};

test('"scanned portion" only when the listing stopped at the cap', () => {
  expect(headlineFor(base)).toBe('Verified in sync');
  const unread = { ...base, in_sync: false, truncated: true, cap_hit: false, unresolved: 3, verdict: 'incomplete' as const };
  // The whole mirror was listed: no "(scanned portion)" next to a body that says so.
  expect(headlineFor(unread)).toBe('Not fully verified');
  expect(headlineFor({ ...unread, cap_hit: true })).toBe('Not fully verified (scanned portion)');
  // A legacy row without the cause fields keeps the hedge.
  expect(headlineFor({ ...unread, cap_hit: undefined, unresolved: undefined })).toBe(
    'Not fully verified (scanned portion)',
  );
});

test('the audit buttons are off with a reason while a run of the rule is live', () => {
  for (const st of ['running', 'cancelling', 'queued']) {
    expect(auditBlockedReason(st)).toMatch(/run of this rule is in progress/);
  }
  for (const st of ['idle', 'succeeded', 'paused', 'failed', undefined]) {
    expect(auditBlockedReason(st)).toBeNull();
  }
});
