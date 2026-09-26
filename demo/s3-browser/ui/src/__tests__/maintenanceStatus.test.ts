import assert from 'node:assert/strict';
import { test } from 'vitest';
import { activePercent, phaseLabel, browserBannerText, type MaintenanceJobView } from '../maintenanceStatus';

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
  assert.ok(browserBannerText(job()).includes('49%'));
  assert.ok(browserBannerText(job()).includes('readable'));
  assert.ok(!browserBannerText(job({ status: 'queued', percent: null })).includes('%'), 'no % when indeterminate');
});
