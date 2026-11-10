/**
 * The bucket's running delete jobs: one line each, Cancel only for the jobs
 * the server says the session may stop (`may_cancel`).
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import DeleteJobsBanner from '../components/DeleteJobsBanner';
import type { MaintenanceJobView } from '../maintenanceStatus';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const job = (over: Partial<MaintenanceJobView>): MaintenanceJobView => ({
  id: 1,
  kind: 'delete',
  bucket: 'releases',
  status: 'running',
  phase: 'deleting',
  objects_total: 12_727,
  objects_done: 3_400,
  objects_skipped: 0,
  objects_failed: 0,
  bytes_done: 0,
  percent: null,
  triggered_by: 'dana',
  created_at: 0,
  started_at: 1,
  finished_at: null,
  ...over,
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

test('one line per job, Cancel only where the session may stop it, and a cancel request', async () => {
  http.on('POST', '/_/api/admin/objects/delete-jobs/1/cancel', json({ status: 'cancelling' }));
  const onChanged = vi.fn();
  const user = userEvent.setup();
  renderWithQuery(
    <DeleteJobsBanner
      jobs={[job({ may_cancel: true }), job({ id: 2, triggered_by: 'ci-uploader', objects_done: 0, may_cancel: false })]}
      me="dana"
      onChanged={onChanged}
    />,
  );
  const banner = screen.getByRole('status', { name: 'Running deletes' });
  expect(within(banner).getByText('Deleting 3,400 of 12,727 objects · 26%')).toBeInTheDocument();
  expect(within(banner).getByText('Deleting 0 of 12,727 objects · 0% · started by ci-uploader')).toBeInTheDocument();
  expect(within(banner).queryByRole('button', { name: 'Cancel delete 2' })).toBeNull();
  await user.click(within(banner).getByRole('button', { name: 'Cancel delete 1' }));
  await waitFor(() => expect(onChanged).toHaveBeenCalledTimes(1));
  expect(http.callsTo('POST', '/_/api/admin/objects/delete-jobs/1/cancel')).toHaveLength(1);
});

test('no running delete: no banner', () => {
  renderWithQuery(<DeleteJobsBanner jobs={[]} me={null} onChanged={() => {}} />);
  expect(screen.queryByRole('status', { name: 'Running deletes' })).toBeNull();
});

test('a job that is stopping or waiting says so, with no Cancel', () => {
  renderWithQuery(
    <DeleteJobsBanner
      jobs={[
        job({ status: 'cancelling', may_cancel: true }),
        job({ id: 2, status: 'queued', started_at: null, may_cancel: true }),
      ]}
      me="dana"
      onChanged={() => {}}
    />,
  );
  expect(screen.getByText('Stopping the delete…')).toBeInTheDocument();
  expect(screen.getByText('Delete waiting to start…')).toBeInTheDocument();
});

test('a job that has not saved its total yet says it is starting', () => {
  renderWithQuery(
    <DeleteJobsBanner jobs={[job({ objects_total: null, objects_done: 0 })]} me="dana" onChanged={() => {}} />,
  );
  expect(screen.getByText('Starting the delete…')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Cancel delete 1' })).toBeNull();
});

test('Cancel follows the server flag, never the name of the user who started the job', () => {
  // `triggered_by` equals `me` on both jobs. Only the flag decides.
  renderWithQuery(
    <DeleteJobsBanner
      jobs={[
        job({ id: 1, triggered_by: 'admin', may_cancel: false }),
        job({ id: 2, triggered_by: 'dana', may_cancel: true }),
        job({ id: 3, triggered_by: 'dana' }),
      ]}
      me="admin"
      onChanged={() => {}}
    />,
  );
  expect(screen.queryByRole('button', { name: 'Cancel delete 1' })).toBeNull();
  expect(screen.getByRole('button', { name: 'Cancel delete 2' })).toBeInTheDocument();
  // A server that sends no flag (an older one) shows no Cancel.
  expect(screen.queryByRole('button', { name: 'Cancel delete 3' })).toBeNull();
});
