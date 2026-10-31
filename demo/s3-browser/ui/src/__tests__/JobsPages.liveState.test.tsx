/**
 * A rule page shows runs that the SERVER starts a moment after an action
 * and reads the Runs tab again when a run finished between two polls. The
 * Jobs page opens a deep link to a one-off that its list does not carry,
 * and asks before it cancels a one-off.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import JobsPanel from '../components/jobs/JobsPanel';
import { LifecycleRulesPanel, ReplicationRulesPanel } from '../components/jobs/RulesPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const JOBS = '/_/api/admin/jobs';
const RUNS = '/_/api/admin/jobs/runs';

function rule(over: Partial<JobRow> = {}): JobRow {
  return {
    id: 'replication:nightly-dr',
    kind: 'replication',
    name: 'nightly-dr',
    scope: { bucket: 'releases', target: 'db-archive' },
    trigger: 'continuous',
    enabled: true,
    paused: false,
    status: 'idle',
    status_raw: 'idle',
    progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
    last_run_at: 100,
    detail: {},
    ...over,
  };
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  http = mockFetch();
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

function rowOf(name: string): HTMLElement {
  const r = screen
    .getAllByText(name)
    .map((el) => el.closest('[role="row"]'))
    .find((x): x is HTMLElement => x !== null);
  if (!r) throw new Error(`no row ${name}`);
  return r;
}

// Regression: after Resume the scheduler started the run 0.5 s later, but
// the list went back to its idle poll and the row stayed idle.
test('after Resume the list polls fast until the run the scheduler starts is seen', async () => {
  let reads = 0;
  http.on('GET', JOBS, () => {
    reads += 1;
    // Read 1: paused. Read 2 (the refetch right after Resume): not started
    // yet. Read 3+: the scheduler started the run.
    if (reads === 1) return json({ jobs: [rule({ paused: true })] });
    if (reads === 2) return json({ jobs: [rule()] });
    return json({ jobs: [rule({ status: 'running', progress: { processed: 4, bytes: 0, failed: 0, skipped: 0 } })] });
  });
  http.on('POST', '/_/api/admin/jobs/replication%3Anightly-dr/resume', json({}, 200));
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  renderWithQuery(<ReplicationRulesPanel />);
  await screen.findByText('nightly-dr');
  await user.click(within(rowOf('nightly-dr')).getByRole('button', { name: 'Resume: nightly-dr' }));
  await waitFor(() => expect(reads).toBe(2));
  // Well inside the 10 s idle poll: the watch polls every 2 s.
  await advance(2100);
  await waitFor(() => expect(within(rowOf('nightly-dr')).getByText(/^running · 4 copied/)).toBeInTheDocument());
});

// Regression: the Runs tab refetched only on an active→terminal edge it
// saw; a run that started and finished between two polls stayed "No runs yet".
test('the Runs tab reads again when last_run_at moves', async () => {
  let lastRun = 100;
  http.on('GET', JOBS, () => json({ jobs: [rule({ last_run_at: lastRun })] }));
  let runs: unknown[] = [];
  http.on('GET', '/_/api/admin/jobs/replication%3Anightly-dr/runs', () => json({ runs }));
  http.on('GET', '/_/api/admin/jobs/replication%3Anightly-dr/failures', json({ failures: [] }));
  http.on('GET', '/_/api/admin/jobs/replication%3Anightly-dr/verify', json({ status: 'idle' }));
  renderWithQuery(<ReplicationRulesPanel search="?job=replication:nightly-dr&tab=runs" />);
  expect(await screen.findByText('No runs yet')).toBeInTheDocument();

  runs = [
    {
      id: 1,
      triggered_by: 'scheduler',
      started_at: 150,
      finished_at: 165,
      status: 'succeeded',
      status_raw: 'succeeded',
      objects_scanned: 3,
      objects_processed: 2,
      objects_skipped: 1,
      objects_deleted: 1,
      bytes: 2048,
      errors: 0,
    },
  ];
  lastRun = 165;
  await advance(10_100);
  expect(await screen.findByText('2 copied · 1 deleted · 1 already in sync')).toBeInTheDocument();
  // Runs rows say how long the run took and how much data it moved.
  expect(screen.getByText('15s')).toBeInTheDocument();
  expect(screen.getByText('2.0 KiB')).toBeInTheDocument();
});

// Regression: ?job=maintenance:3 for a job the list did not carry read
// "Lifecycle · 3 [draft] — no longer exists".
test('a deep link to a one-off the list does not carry loads it by id', async () => {
  http.on('GET', RUNS, json({ runs: [] }));
  http.on(
    'GET',
    '/_/api/admin/jobs/maintenance%3A3',
    json({
      ...rule(),
      id: 'maintenance:3',
      kind: 'backfill-metadata',
      name: 'downloads',
      scope: { bucket: 'downloads' },
      trigger: 'oneoff',
      status: 'failed',
      status_raw: 'failed',
      last_error: 'backend unreachable',
      detail: { refresh_last_modified: false },
    }),
  );
  http.on('GET', '/_/api/admin/jobs/maintenance%3A3/runs', json({ runs: [] }));
  http.on('GET', '/_/api/admin/jobs/maintenance%3A3/failures', json({ failures: [] }));
  renderWithQuery(<JobsPanel search="?job=maintenance:3" />);
  const dialog = await screen.findByRole('dialog');
  expect(await within(dialog).findByText(/Backfill metadata/)).toBeInTheDocument();
  expect(within(dialog).queryByText('draft')).not.toBeInTheDocument();
  expect(within(dialog).queryByText(/Lifecycle/)).not.toBeInTheDocument();
  expect(within(dialog).getByText('kept as it was')).toBeInTheDocument();
  expect(within(dialog).getByText('backend unreachable')).toBeInTheDocument();
});

// A pre-flip cancel deletes every staged copy: one click must not do it.
test('Cancel of a running migration asks first and says what it deletes', async () => {
  const migrate: JobRow = {
    ...rule(),
    id: 'maintenance:9',
    kind: 'migrate',
    name: 'releases',
    scope: { bucket: 'releases', target: 'aws-dr' },
    trigger: 'oneoff',
    status: 'running',
    status_raw: 'running',
    phase: 'copy',
    started_at: 100,
    progress: { processed: 20_001, bytes: 0, failed: 0, skipped: 0 },
    detail: { target_backend: 'aws-dr', from_backend: 'hetzner-fsn1', delete_source: false, target: 'empty' },
  };
  http.on('GET', RUNS, json({ runs: [{ ...migrate, job_id: 'maintenance:9', origin: 'admin' }] }));
  http.on('POST', '/_/api/admin/jobs/maintenance%3A9/cancel', json({}, 200));
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  renderWithQuery(<JobsPanel />);
  await screen.findByText('releases');
  const row = rowOf('releases');
  expect(within(row).getByText(/^copying · 20,001 copied/)).toBeInTheDocument();
  // No bare cancel icon on the row: Cancel lives behind "⋯" with a confirmation.
  expect(within(row).queryByRole('button', { name: /^Cancel/ })).not.toBeInTheDocument();
  await user.click(within(row).getByRole('button', { name: 'More actions for releases' }));
  await user.click(await screen.findByText('Cancel…'));
  expect((await screen.findAllByText('Cancel the migration of "releases"?')).length).toBeGreaterThan(0);
  expect(screen.getAllByText(/deletes the copies it made on the target backend/).length).toBeGreaterThan(0);
  expect(http.callsTo('POST', '/_/api/admin/jobs/maintenance%3A9/cancel')).toHaveLength(0);
  await user.click(screen.getByRole('button', { name: 'Cancel job' }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/jobs/maintenance%3A9/cancel')).toHaveLength(1));
});

// Regression: the floating unsaved-changes bar covered the last fields of
// the create drawer (Action / Destination), then sat half under the drawer's
// edge. It now lives in the drawer footer, out of the body's way.
test('while edits are unsaved, the dirty bar is in the drawer footer', async () => {
  http.on('GET', JOBS, json({ jobs: [] }));
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  renderWithQuery(<LifecycleRulesPanel />);
  await user.click(await screen.findByRole('button', { name: /New lifecycle rule/ }));
  const footer = (await screen.findByRole('dialog')).querySelector('.ant-drawer-footer') as HTMLElement;
  expect(within(footer).getByRole('button', { name: 'Review & apply' })).toBeInTheDocument();
});

// Regression (QA nightly step 7): the header and the empty list both said
// "No jobs yet".
test('an empty Jobs screen says "No runs yet" once', async () => {
  http.on('GET', RUNS, json({ runs: [] }));
  renderWithQuery(<JobsPanel />);
  expect(await screen.findAllByText(/No runs yet/)).toHaveLength(1);
});
