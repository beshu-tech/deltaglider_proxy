/**
 * JobsPanel run-now: the button POSTs /jobs/:id/run-now, the server answers
 * 202 (the run goes on in the background), the panel refetches the list at
 * once, and then polls fast (2s) while the row is live until it settles.
 * With nothing live the list polls every 10s.
 *
 * Also pins ccb32e65: run-now waits while the rule holds unsaved edits (it
 * would run the SAVED rule) — and only that rule, not every rule of its kind.
 */
import { act, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import { ReplicationRulesPanel } from '../components/jobs/RulesPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const JOBS = '/_/api/admin/jobs';
const RUN_NOW = '/_/api/admin/jobs/replication%3Anightly-dr/run-now';

function replicationRow(status: string, over: Partial<JobRow> = {}): JobRow {
  return {
    id: 'replication:nightly-dr',
    kind: 'replication',
    name: 'nightly-dr',
    scope: { bucket: 'releases', target: 'db-archive' },
    trigger: 'continuous',
    enabled: true,
    paused: false,
    status,
    status_raw: status,
    progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
    detail: {},
    ...over,
  };
}

let http: ReturnType<typeof mockFetch>;
/** Server-side status of the rule; each GET /jobs reads it. */
let serverStatus: string;
/** Statuses the next GETs return, in order, before falling back to serverStatus. */
let queued: string[];

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  serverStatus = 'idle';
  queued = [];
  http = mockFetch();
  http.on('GET', JOBS, () => json({ jobs: [replicationRow(queued.shift() ?? serverStatus)] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

async function mount() {
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  renderWithQuery(<ReplicationRulesPanel />);
  await screen.findByText('nightly-dr');
  return user;
}

/** The record-list row for the rule. */
function jobRow(): HTMLElement {
  const row = screen
    .getAllByText('nightly-dr')
    .map((el) => el.closest('[role="row"]'))
    .find((r): r is HTMLElement => r !== null);
  if (!row) throw new Error('no job row');
  return row;
}

async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

describe('run-now', () => {
  test('POSTs run-now, accepts the 202, refetches, and polls until the run settles', async () => {
    const user = await mount();
    expect(within(jobRow()).getByText('idle')).toBeInTheDocument();

    http.on('POST', RUN_NOW, () => {
      // The run starts in the background: the next list read is live, the
      // one after it has finished.
      queued = ['running'];
      serverStatus = 'succeeded';
      return json({ started: true }, 202);
    });
    const getsBefore = http.callsTo('GET', JOBS).length;
    await user.click(within(jobRow()).getByRole('button', { name: 'Run now: nightly-dr' }));

    expect(http.callsTo('POST', RUN_NOW)).toHaveLength(1);
    expect(await screen.findByText('Run started — progress shows in the row and the Runs tab')).toBeInTheDocument();

    // Immediate refetch (invalidate), not the idle poll. A running rule shows
    // its live meter.
    await waitFor(() => expect(within(jobRow()).getByText(/^running · 0 copied/)).toBeInTheDocument());
    expect(http.callsTo('GET', JOBS).length).toBe(getsBefore + 1);
    // A running replication rule offers kill, not another run-now.
    expect(within(jobRow()).queryByRole('button', { name: 'Run now: nightly-dr' })).not.toBeInTheDocument();

    // While live, the list polls every 2s with no user action.
    await advance(2100);
    await waitFor(() => expect(within(jobRow()).getByText('succeeded')).toBeInTheDocument());
    expect(within(jobRow()).getByRole('button', { name: 'Run now: nightly-dr' })).toBeInTheDocument();
  });

  test('a settled list goes back to the slow poll: no refetch within 5s', async () => {
    await mount();
    const gets = http.callsTo('GET', JOBS).length;
    await advance(5000);
    expect(http.callsTo('GET', JOBS).length).toBe(gets);
  });

  test('a rejected run-now (409) shows the server error and leaves the row as it was', async () => {
    const user = await mount();
    http.on('POST', RUN_NOW, json({ error: 'rule is already running' }, 409));
    await user.click(within(jobRow()).getByRole('button', { name: 'Run now: nightly-dr' }));
    expect(await screen.findByText(/rule is already running/)).toBeInTheDocument();
    expect(within(jobRow()).getByText('idle')).toBeInTheDocument();
  });

  test('a disabled rule offers "Run once" (runs a single time, does not enable it)', async () => {
    http.on('GET', JOBS, json({ jobs: [replicationRow('idle', { enabled: false })] }));
    await mount();
    expect(
      within(jobRow()).getByRole('button', { name: 'Run this rule once now — does not enable or resume it: nightly-dr' }),
    ).toHaveTextContent('Run once');
  });

  test('run-now waits while THIS rule has unsaved edits — not while another rule does', async () => {
    http.on(
      'GET',
      '/_/api/admin/config/section/storage',
      json({
        replication: {
          enabled: true,
          rules: [
            {
              name: 'nightly-dr',
              enabled: true,
              source: { bucket: 'releases', prefix: '' },
              destination: { bucket: 'db-archive', prefix: '' },
            },
          ],
        },
      }),
    );
    const user = await mount();
    const runNow = () => within(jobRow()).getByRole('button', { name: 'Run now: nightly-dr' });
    // "New job → Replication rule" adds a draft rule: the editor is dirty,
    // but nightly-dr itself is unchanged, so its Run now stays.
    await user.click(await screen.findByRole('button', { name: /New replication rule/ }));
    expect(await screen.findAllByText('draft')).not.toHaveLength(0);
    expect(runNow()).toBeEnabled();
    expect(within(jobRow()).queryByText('edited')).not.toBeInTheDocument();

    // Edit nightly-dr itself: now its Run now waits, and the row says why.
    await user.click(within(jobRow()).getByText('nightly-dr'));
    const enabled = await screen.findByRole('switch', { name: /Enabled/ });
    await user.click(enabled);
    await waitFor(() => expect(runNow()).toBeDisabled());
    expect(runNow()).toHaveAttribute(
      'title',
      'Apply or discard your unsaved edits to this rule first — this would use the saved rule.',
    );
    expect(within(jobRow()).getByText('edited')).toBeInTheDocument();
    expect(http.callsTo('POST', RUN_NOW)).toHaveLength(0);
  });
});
