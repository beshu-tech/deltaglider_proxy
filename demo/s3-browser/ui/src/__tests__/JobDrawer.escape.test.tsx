/**
 * Escape inside an open bucket autocomplete in the job drawer closes the
 * dropdown only; a second Escape closes the drawer. Regression: the first
 * Escape closed the whole drawer (the drawer listens for Escape on window).
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  const http = mockFetch();
  http.on(
    'GET',
    '/_/api/admin/jobs',
    json({
      jobs: [
        {
          id: 'lifecycle:expire-old',
          kind: 'lifecycle',
          name: 'expire-old',
          scope: { bucket: 'releases' },
          trigger: 'scheduled',
          enabled: true,
          paused: false,
          status: 'idle',
          status_raw: 'idle',
          progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
          detail: {},
        },
      ],
    }),
  );
  http.on(
    'GET',
    '/_/api/admin/config/section/storage',
    json({
      lifecycle: {
        enabled: true,
        rules: [{ name: 'expire-old', enabled: true, bucket: 'releases', prefix: '', action: 'delete', expire_after: '30d' }],
      },
    }),
  );
  http.on('GET', '/_/api/admin/jobs/lifecycle%3Aexpire-old/runs', json({ runs: [] }));
  http.on('GET', '/_/api/admin/jobs/lifecycle%3Aexpire-old/failures', json({ failures: [] }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [{ name: 'releases' }, { name: 'downloads' }] }));
});
afterEach(() => vi.unstubAllGlobals());

test('Escape in an open autocomplete does not close the drawer', async () => {
  const user = userEvent.setup();
  renderWithQuery(<JobsPanel />);
  await user.click(await screen.findByText('expire-old'));
  await screen.findByRole('dialog');
  const [bucket] = await screen.findAllByRole('combobox');
  await user.click(bucket);
  await user.clear(bucket);
  await user.type(bucket, 'd');
  await waitFor(() => expect(bucket).toHaveAttribute('aria-expanded', 'true'));

  await user.keyboard('{Escape}');
  expect(bucket).toHaveAttribute('aria-expanded', 'false');
  expect(screen.getByRole('dialog')).toBeInTheDocument();

  await user.keyboard('{Escape}');
  await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
});
