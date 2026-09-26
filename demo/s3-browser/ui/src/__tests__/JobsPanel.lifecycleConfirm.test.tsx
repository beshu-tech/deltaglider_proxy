/**
 * Lifecycle run-now asks first, and preview stays on screen.
 *
 * Pins the browser-review finding: "Run now" on a lifecycle rule deleted at
 * once, with no confirmation, and "Preview" was a toast that vanished. Now
 * run-now opens a dialog that lists the preview candidates (count and bytes)
 * and runs only on its "Run" button, which names the count. Preview opens
 * the drawer's Preview tab with the same list.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const JOBS = '/_/api/admin/jobs';
const ID = 'lifecycle%3Aexpire-nightlies';
const RUN_NOW = `/_/api/admin/jobs/${ID}/run-now`;
const PREVIEW = `/_/api/admin/jobs/${ID}/preview`;

const row: JobRow = {
  id: 'lifecycle:expire-nightlies',
  kind: 'lifecycle',
  name: 'expire-nightlies',
  scope: { bucket: 'releases' },
  trigger: 'scheduled',
  enabled: true,
  paused: false,
  status: 'idle',
  status_raw: 'idle',
  progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
  detail: {},
};

const candidate = (key: string, size: number) => ({
  bucket: 'releases', key, action: 'delete', delete_source_after_success: false, created_at: '2026-01-01T00:00:00Z', size,
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', JOBS, json({ jobs: [row] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('POST', PREVIEW, json({
    rule_name: 'expire-nightlies', status: 'preview', objects_scanned: 40, objects_affected: 2, objects_skipped: 0,
    bytes_affected: 3 * 1024 * 1024, errors: 0,
    candidates: [candidate('nightly/app-0.9.tar', 1024 * 1024), candidate('nightly/app-0.8.tar', 2 * 1024 * 1024)],
    failures: [],
  }));
  http.on('POST', RUN_NOW, json({ started: true }, 202));
});
afterEach(() => {
  vi.unstubAllGlobals();
});

function jobRow(): HTMLElement {
  return screen.getByText('expire-nightlies').closest('[role="row"]') as HTMLElement;
}

test('run-now on a lifecycle rule confirms with the candidate list before it runs', async () => {
  const user = userEvent.setup();
  renderWithQuery(<JobsPanel />);
  await screen.findByText('expire-nightlies');
  await user.click(within(jobRow()).getByRole('button', { name: 'Run now' }));

  const dialog = await screen.findByRole('dialog');
  expect(await within(dialog).findByText('nightly/app-0.9.tar')).toBeInTheDocument();
  expect(within(dialog).getByText('nightly/app-0.8.tar')).toBeInTheDocument();
  expect(within(dialog).getByText(/3(\.0)? MB/)).toBeInTheDocument();
  expect(http.callsTo('POST', RUN_NOW)).toHaveLength(0);

  await user.click(within(dialog).getByRole('button', { name: 'Run: delete 2 objects' }));
  await waitFor(() => expect(http.callsTo('POST', RUN_NOW)).toHaveLength(1));
});

test('cancelling the confirmation never sends run-now', async () => {
  const user = userEvent.setup();
  renderWithQuery(<JobsPanel />);
  await screen.findByText('expire-nightlies');
  await user.click(within(jobRow()).getByRole('button', { name: 'Run now' }));
  const dialog = await screen.findByRole('dialog');
  await within(dialog).findByText('nightly/app-0.9.tar');
  await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
  await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  expect(http.callsTo('POST', RUN_NOW)).toHaveLength(0);
});

test('preview shows the candidate list in the drawer, not a toast', async () => {
  const user = userEvent.setup();
  renderWithQuery(<JobsPanel />);
  await screen.findByText('expire-nightlies');
  await user.click(within(jobRow()).getByRole('button', { name: 'Preview' }));
  const tab = await screen.findByRole('tab', { name: 'Preview', selected: true });
  expect(tab).toBeInTheDocument();
  expect(await screen.findByText('nightly/app-0.9.tar')).toBeInTheDocument();
  // Still there after a while: it is a view, not a notification.
  await new Promise((r) => setTimeout(r, 100));
  expect(screen.getByText('nightly/app-0.9.tar')).toBeInTheDocument();
});
