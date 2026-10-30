/**
 * The Buckets "busy" chip follows the server's write gate
 * (`writes_blocked`), not the job alone: a migration that is being
 * cancelled has opened the bucket again while its row is still active.
 * Its Cancel asks first, because a pre-switch cancel deletes the copies.
 */
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import BucketCard from '../components/BucketCard';
import type { JobRow } from '../jobsView';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const job: JobRow = {
  id: 'maintenance:4',
  kind: 'migrate',
  name: 'releases',
  scope: { bucket: 'releases', target: 'aws-dr' },
  trigger: 'oneoff',
  status: 'running',
  status_raw: 'running',
  phase: 'copy',
  percent: null,
  progress: { processed: 10, bytes: 0, failed: 0, skipped: 0 },
  detail: {},
};

afterEach(() => vi.unstubAllGlobals());

function renderCard(writesBlocked: boolean) {
  const http = mockFetch();
  http.on(
    'GET',
    '/_/api/admin/jobs/bucket/releases',
    json({ active: { kind: 'migrate', status: 'running', writes_blocked: writesBlocked }, writes_blocked: writesBlocked }),
  );
  renderWithQuery(
    <BucketCard
      name="releases"
      row={null}
      real
      expanded={false}
      onToggle={() => {}}
      backends={[]}
      defaultBackend="local-disk"
      originBackend="local-disk"
      globalCompressionOn
      globalRatio={0.75}
      onPatch={() => {}}
      onPrefixesChange={() => {}}
      inputRadius={{ borderRadius: 6 }}
      maintenanceJob={job}
    />,
  );
  return http;
}

test('gate armed: busy', async () => {
  renderCard(true);
  expect(await screen.findByText(/Busy/)).toBeInTheDocument();
});

test('gate open while the job row is active: not busy', async () => {
  renderCard(false);
  expect(await screen.findByText(/Job finishing/)).toBeInTheDocument();
  expect(screen.queryByText(/Busy/)).toBeNull();
});

test('Cancel asks before it cancels', async () => {
  const http = renderCard(true);
  http.on('POST', '/_/api/admin/jobs/maintenance%3A4/cancel', json({}));
  const user = userEvent.setup();
  await user.click(screen.getByRole('button', { name: 'Cancel' }));
  const dialog = await screen.findByRole('dialog');
  expect(within(dialog).getAllByText(/deletes the copies/).length).toBeGreaterThan(0);
  expect(http.callsTo('POST', '/_/api/admin/jobs/maintenance%3A4/cancel')).toHaveLength(0);
  await user.click(within(dialog).getByRole('button', { name: 'Cancel job' }));
  await vi.waitFor(() => expect(http.callsTo('POST', '/_/api/admin/jobs/maintenance%3A4/cancel')).toHaveLength(1));
});
