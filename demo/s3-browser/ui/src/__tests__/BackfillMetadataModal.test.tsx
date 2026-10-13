/**
 * Jobs QA (overview, polish): the backfill dialog listed every bucket with no
 * search and no backend, Start sat far below the list, and after a partial
 * start the started buckets stayed checked, so a second Start sent them
 * again. Now: a search box, the backend per bucket, a list that scrolls on
 * its own, and only the failed buckets stay checked.
 */
import { fireEvent, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackfillMetadataModal from '../components/BackfillMetadataModal';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const START = '/_/api/admin/jobs/backfill-metadata';
let http: ReturnType<typeof mockFetch>;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', '/_/api/admin/buckets', json({
    buckets: [
      { name: 'releases', backend_name: 'hetzner-fsn1' },
      { name: 'db-archive', backend_name: 'aws-dr' },
      { name: 'downloads', backend_name: 'local-disk' },
    ],
  }));
});
afterEach(() => vi.unstubAllGlobals());

test('each bucket shows its backend; the search filters by bucket or backend', async () => {
  renderWithQuery(<BackfillMetadataModal open onClose={() => {}} />);
  expect(await screen.findByRole('checkbox', { name: 'releases' })).toBeInTheDocument();
  expect(screen.getByText('on hetzner-fsn1')).toBeInTheDocument();
  expect(screen.getByText('on local-disk')).toBeInTheDocument();
  fireEvent.change(screen.getByRole('searchbox', { name: 'Search buckets' }), { target: { value: 'local' } });
  expect(screen.getByRole('checkbox', { name: 'downloads' })).toBeInTheDocument();
  expect(screen.queryByRole('checkbox', { name: 'releases' })).toBeNull();
  fireEvent.change(screen.getByRole('searchbox', { name: 'Search buckets' }), { target: { value: 'db-' } });
  expect(screen.getByRole('checkbox', { name: 'db-archive' })).toBeInTheDocument();
  expect(screen.queryByRole('checkbox', { name: 'downloads' })).toBeNull();
});

test('after a partial start only the failed buckets stay checked', async () => {
  http.on('POST', START, json({
    started: [{ bucket: 'releases', job_id: 3 }],
    errors: [{ bucket: 'downloads', error: 'a job already runs on this bucket' }],
  }));
  const onClose = vi.fn();
  renderWithQuery(<BackfillMetadataModal open onClose={onClose} />);
  fireEvent.click(await screen.findByRole('checkbox', { name: 'releases' }));
  fireEvent.click(screen.getByRole('checkbox', { name: 'downloads' }));
  fireEvent.click(screen.getByRole('button', { name: /Start now \(2 buckets\)/ }));
  await waitFor(() => expect(screen.getByRole('button', { name: /Start now \(1 bucket\)/ })).toBeInTheDocument(), { timeout: 5000 });
  expect(screen.getByRole('checkbox', { name: 'releases' })).not.toBeChecked();
  expect(screen.getByRole('checkbox', { name: 'downloads' })).toBeChecked();
  expect(onClose).not.toHaveBeenCalled();
  // A second Start sends only the failed bucket (once the first request
  // ended: a loading button ignores the click).
  await waitFor(
    () => expect(screen.getByRole('button', { name: /Start now \(1 bucket\)/ })).not.toHaveClass('ant-btn-loading'),
    { timeout: 5000 },
  );
  fireEvent.click(screen.getByRole('button', { name: /Start now \(1 bucket\)/ }));
  await waitFor(() => expect(http.callsTo('POST', START)).toHaveLength(2), { timeout: 5000 });
  expect(http.callsTo('POST', START)[1].body).toMatchObject({ buckets: ['downloads'] });
});
