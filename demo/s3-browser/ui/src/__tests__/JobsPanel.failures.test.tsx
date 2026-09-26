/**
 * A finished one-off job with per-object failures shows the failure count in
 * its row, and the count opens the drawer on the Failures tab.
 */
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { JobRow } from '../jobsView';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const row: JobRow = {
  id: 'maintenance:7',
  kind: 'reencrypt',
  name: 'db-archive',
  scope: { bucket: 'db-archive' },
  trigger: 'oneoff',
  status: 'completed_with_errors',
  status_raw: 'completed_with_errors',
  progress: { processed: 40, bytes: 1024, failed: 3, skipped: 0 },
  last_error: "3 object(s) failed — see the job's Failures tab",
  detail: {},
};

beforeEach(() => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [row] }));
  http.on('GET', '/_/api/admin/jobs/maintenance%3A7/runs', json({ runs: [] }));
  http.on(
    'GET',
    '/_/api/admin/jobs/maintenance%3A7/failures',
    json({ failures: [{ id: 1, occurred_at: 1, object_key: 'nightly/2026-06-10.dump', error: 'decrypt failed' }] }),
  );
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('the row shows the failure count and it opens the Failures tab', async () => {
  const user = userEvent.setup();
  renderWithQuery(<JobsPanel />);
  const link = await screen.findByRole('button', { name: '3 failed — show failures: db-archive' });
  expect(link.closest('[role="row"]')).not.toBeNull();
  expect(within(link).getByText('3 failed')).toBeInTheDocument();
  await user.click(link);
  expect(await screen.findByRole('tab', { name: 'Failures', selected: true })).toBeInTheDocument();
  expect(await screen.findByText('nightly/2026-06-10.dump')).toBeInTheDocument();
});
