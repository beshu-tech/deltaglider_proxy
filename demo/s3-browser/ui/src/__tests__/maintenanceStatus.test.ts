import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  activePercent,
  phaseLabel,
  browserBannerText,
  bucketStatusPollMs,
  bucketWritesBlocked,
  type MaintenanceJobView,
} from '../maintenanceStatus';

const job = (over: Partial<MaintenanceJobView> = {}): MaintenanceJobView => ({
  id: 1,
  kind: 'reencrypt',
  bucket: 'pippo',
  status: 'running',
  phase: 'objects',
  objects_total: 100,
  objects_done: 40,
  objects_skipped: 10,
  objects_failed: 0,
  bytes_done: 1234,
  percent: 49,
  triggered_by: 'admin',
  created_at: 1,
  started_at: 2,
  finished_at: null,
  ...over,
});

// (isActiveStatus was deleted — the live-status check is jobsView.isActiveJobStatus,
//  covered by test:jobs-view.)

test('activePercent', () => {
  assert.equal(activePercent(job()), 49);
  assert.equal(activePercent(job({ status: 'queued', percent: null })), null, 'queued = indeterminate');
  assert.equal(activePercent(job({ phase: 'counting', percent: null })), null, 'counting = indeterminate');
});

test('phaseLabel', () => {
  assert.equal(phaseLabel(job({ status: 'queued' })), 'Waiting to start…');
  assert.equal(phaseLabel(job({ status: 'cancelling' })), 'Cancelling…');
  assert.equal(phaseLabel(job({ phase: 'counting' })), 'Counting objects…');
  assert.equal(phaseLabel(job()), '50 / 100 objects');
  assert.equal(phaseLabel(job({ objects_total: null })), '50 objects');
  assert.ok(phaseLabel(job({ phase: 'references' })).startsWith('Finalizing'));
});

test('browserBannerText', () => {
  assert.ok(browserBannerText(job(), true).includes('49%'));
  assert.ok(browserBannerText(job(), true).includes('readable'));
  assert.ok(!browserBannerText(job({ status: 'queued', percent: null }), true).includes('%'), 'no % when indeterminate');
});

// Regression: while a migration was cancelling, the gate was already open
// (PUT 200) but the banner said uploads were unavailable and Upload was
// hidden. The answer is the server's `writes_blocked`, never a guess from
// kind or phase.
test('bucketWritesBlocked follows the server, not the kind or phase', () => {
  assert.equal(bucketWritesBlocked(null), false);
  assert.equal(bucketWritesBlocked({ active: null, writes_blocked: false }), false);
  // A running migrate in its copy phase, but the gate is open: writable.
  const copying = job({ kind: 'migrate', phase: 'copy' });
  assert.equal(bucketWritesBlocked({ active: copying, writes_blocked: false }), false);
  // A cancelling migrate whose gate is still armed: blocked.
  const cancelling = job({ kind: 'migrate', phase: 'copy', status: 'cancelling' });
  assert.equal(bucketWritesBlocked({ active: cancelling, writes_blocked: true }), true);
  // Only the copy inside `active` carries the answer.
  assert.equal(bucketWritesBlocked({ active: job({ writes_blocked: false }) }), false);
  // A server without the field gated every active job.
  assert.equal(bucketWritesBlocked({ active: cancelling }), true);
  assert.match(browserBannerText(cancelling, false), /Uploads and deletes work again/);
  assert.doesNotMatch(browserBannerText(cancelling, false), /unavailable/);
  assert.match(browserBannerText(cancelling, true), /work again when it stops/);
  assert.match(browserBannerText(copying, false), /uploads and deletes work\./);
});

test('phaseLabel: migrate phase words', () => {
  assert.equal(phaseLabel(job({ kind: 'migrate', phase: 'verify' })), 'Verifying the copies…');
  assert.equal(phaseLabel(job({ kind: 'migrate', phase: 'copy', objects_done: 1200, objects_skipped: 0 })), 'Copying · 1,200 objects');
  assert.equal(phaseLabel(job({ kind: 'migrate', phase: 'flip' })), 'Switching to the new backend…');
  assert.ok(browserBannerText(job({ kind: 'backfill-metadata' }), true).startsWith('Adding metadata'));
});

// Regression: after a 401 the poll stopped for good, so the banner froze
// under the session-expired modal and never came back after sign-in.
test('bucketStatusPollMs keeps polling after an error, slower', () => {
  assert.equal(bucketStatusPollMs(true, false, 2000, 10_000), 2000);
  assert.equal(bucketStatusPollMs(false, false, 2000, 10_000), 10_000);
  assert.equal(bucketStatusPollMs(false, true, 2000, 10_000), 30_000);
});
