/**
 * UX harvest D17: a failed bucket list showed "No buckets yet" and a hero of
 * zero figures as facts. The page now shows ONE error with Retry, and no
 * empty state.
 */
import { screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  listBuckets: async () => {
    throw new Error('ListBuckets failed (500) [InternalError]: boom');
  },
}));

import AnalyticsSection from '../components/AnalyticsSection';

beforeEach(() => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/diagnostics/scan/status', json({ buckets: {}, running: {} }));
  http.on('GET', '/_/api/admin/usage/buckets', json({ buckets: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('a failed bucket list shows one error with Retry, not an empty state', async () => {
  renderWithQuery(<AnalyticsSection config={null} />);
  expect(await screen.findByText('Could not load the buckets')).toBeInTheDocument();
  expect(screen.getAllByRole('alert')).toHaveLength(1);
  expect(screen.getByRole('button', { name: /Retry/ })).toBeInTheDocument();
  expect(screen.queryByText('No buckets yet')).toBeNull();
  expect(screen.queryByText(/boom/)).toBeNull();
});
