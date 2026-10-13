/**
 * A settled replication rule shows what its last run did and what its live
 * (event-driven) copies did; its Failures tab says how many failures there
 * are in total and which came from a live copy; its Runs rows say when the
 * run finished.
 */
import { screen, within } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const now = Math.floor(Date.now() / 1000);
const rule: JobRow = {
  id: 'replication:nightly-dr',
  kind: 'replication',
  name: 'nightly-dr',
  scope: { bucket: 'releases', target: 'db-archive' },
  trigger: 'continuous',
  enabled: true,
  paused: false,
  status: 'succeeded',
  status_raw: 'succeeded',
  progress: { processed: 14, skipped: 3, failed: 2, bytes: 4096, deleted: 1 },
  failed_total: 2,
  last_error: 'PUT db-archive/a.bin: 503 SlowDown',
  last_run_at: now - 120,
  detail: { last_event_applied_at: now - 120, events: { copied: 5, deleted: 0, failed: 1 } },
};

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [rule] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on(
    'GET',
    '/_/api/admin/jobs/replication%3Anightly-dr/runs',
    json({
      runs: [
        {
          id: 1, triggered_by: 'scheduler', started_at: now - 180, finished_at: now - 120, status: 'succeeded',
          status_raw: 'succeeded', objects_scanned: 20, objects_processed: 14, objects_skipped: 3, objects_deleted: 1,
          bytes: 4096, errors: 2,
        },
      ],
    }),
  );
  http.on(
    'GET',
    '/_/api/admin/jobs/replication%3Anightly-dr/failures',
    json({
      total: 250,
      failures: [
        { id: 2, occurred_at: now - 60, object_key: 'a.bin', error: 'PUT db-archive/a.bin: 503 SlowDown', trigger: 'event' },
        { id: 1, occurred_at: now - 130, object_key: 'b.bin', error: 'PUT db-archive/b.bin: 503 SlowDown', trigger: 'run' },
      ],
    }),
  );
  http.on('GET', '/_/api/admin/jobs/replication%3Anightly-dr/verify', json({ status: 'idle' }));
});
afterEach(() => vi.unstubAllGlobals());

function ruleRow(): HTMLElement {
  return screen.getAllByText('nightly-dr').map((e) => e.closest('[role="row"]')).find((r): r is HTMLElement => !!r)!;
}

test('a settled rule row keeps its counts and shows its live copies', async () => {
  renderWithQuery(<JobsPanel />);
  await screen.findByText('nightly-dr');
  const r = ruleRow();
  expect(within(r).getByText('14 copied · 1 deleted · 3 already in sync · 2 failed')).toBeInTheDocument();
  expect(within(r).getByText(/last live copy/)).toHaveTextContent(/^last live copy \dm ago · 5 copied · 1 failed$/);
  expect(within(r).getByText('PUT db-archive/a.bin: 503 SlowDown')).toBeInTheDocument();
  expect(within(r).getByRole('button', { name: /2 failed — show failures/ })).toBeInTheDocument();
});

test('Failures: total, live-copy tag; Runs: finish time', async () => {
  renderWithQuery(<JobsPanel search="?job=replication:nightly-dr&tab=failures" />);
  const drawer = await screen.findByRole('dialog');
  expect(await within(drawer).findByText('The 2 most recent of 250 failures.')).toBeInTheDocument();
  expect(within(drawer).getByText('live copy')).toBeInTheDocument();
  expect(within(drawer).getByText('run')).toBeInTheDocument();
});

test('Runs rows say when the run finished', async () => {
  renderWithQuery(<JobsPanel search="?job=replication:nightly-dr&tab=runs" />);
  const drawer = await screen.findByRole('dialog');
  expect(await within(drawer).findByText('Finished')).toBeInTheDocument();
  expect(within(drawer).getByText('1m')).toBeInTheDocument();
});
