/**
 * Lifecycle run-now matches the server: the proxy refuses (409) a run of a
 * disabled or paused lifecycle rule, and every lifecycle run while "Run
 * lifecycle rules on schedule" is off. The row shows "Run now" disabled with
 * a title that says why, never "Run once". Replication keeps its one-off.
 */
import { screen, within } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const JOBS = '/_/api/admin/jobs';
const SECTION = '/_/api/admin/config/section/storage';

function lifecycleRow(over: Partial<JobRow> = {}): JobRow {
  return {
    id: 'lifecycle:expire-nightly-dumps',
    kind: 'lifecycle',
    name: 'expire-nightly-dumps',
    scope: { bucket: 'db-archive', prefix: 'nightly/' },
    trigger: 'scheduled',
    enabled: true,
    paused: false,
    status: 'idle',
    status_raw: 'idle',
    progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
    detail: {},
    ...over,
  };
}

let http: ReturnType<typeof mockFetch>;
function section(schedulerOn: boolean) {
  return json({ lifecycle: { enabled: schedulerOn, tick_interval: '1h', max_failures_retained: 100, rules: [] } });
}

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', SECTION, section(true));
});
afterEach(() => vi.unstubAllGlobals());

async function runNowButton() {
  renderWithQuery(<JobsPanel />);
  const row = (await screen.findByText('expire-nightly-dumps')).closest('[role="row"]') as HTMLElement;
  return within(row).findByRole('button', { name: 'Run now: expire-nightly-dumps' });
}

test('a disabled lifecycle rule shows Run now disabled, with the reason, not Run once', async () => {
  http.on('GET', JOBS, json({ jobs: [lifecycleRow({ enabled: false })] }));
  const btn = await runNowButton();
  expect(btn).toBeDisabled();
  expect(btn).toHaveTextContent('Run now');
  expect(btn.getAttribute('title')).toMatch(/Enable the rule first/);
  expect(screen.queryByText('Run once')).not.toBeInTheDocument();
});

test('a paused lifecycle rule shows Run now disabled with "Resume the rule first"', async () => {
  http.on('GET', JOBS, json({ jobs: [lifecycleRow({ paused: true })] }));
  const btn = await runNowButton();
  expect(btn).toBeDisabled();
  expect(btn.getAttribute('title')).toMatch(/Resume the rule first/);
});

test('with the lifecycle scheduler off, Run now is disabled and names the switch', async () => {
  http.on('GET', SECTION, section(false));
  http.on('GET', JOBS, json({ jobs: [lifecycleRow()] }));
  const btn = await runNowButton();
  await vi.waitFor(() => expect(btn).toBeDisabled());
  expect(btn.getAttribute('title')).toMatch(/Run lifecycle rules on schedule/);
});

test('an enabled lifecycle rule with the scheduler on keeps Run now enabled', async () => {
  http.on('GET', JOBS, json({ jobs: [lifecycleRow()] }));
  const btn = await runNowButton();
  await vi.waitFor(() => expect(btn).toBeEnabled());
});
