/**
 * Explore finding 19: the backfill-metadata job had an API
 * (POST /jobs/backfill-metadata) and a row label, but no way to start it
 * from the Jobs screen.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [{ name: 'releases' }, { name: 'downloads' }] }));
  http.on(
    'POST',
    '/_/api/admin/jobs/backfill-metadata',
    json({ started: [{ bucket: 'downloads', job_id: 7 }], errors: [] }),
  );
});
afterEach(() => vi.unstubAllGlobals());

test('New job → Backfill metadata starts the job for the chosen buckets', async () => {
  const user = userEvent.setup();
  renderWithQuery(<JobsPanel />);
  await user.click(await screen.findByRole('button', { name: /New job/ }));
  await user.click(await screen.findByText(/Backfill metadata…/));
  const dialog = await screen.findByRole('dialog', { name: /Backfill object metadata/ });
  await user.click(await within(dialog).findByRole('checkbox', { name: 'downloads' }));
  await user.click(within(dialog).getByRole('button', { name: /Start now/ }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/jobs/backfill-metadata')).toHaveLength(1));
  expect(http.callsTo('POST', '/_/api/admin/jobs/backfill-metadata')[0].body).toEqual({
    buckets: ['downloads'],
    refresh_last_modified: false,
  });
});
