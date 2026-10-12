/**
 * The analytics dashboard reads the usage counters of all buckets in one
 * request, not one request per bucket (P8.8a).
 */
import { waitFor } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  listBuckets: async () => ['releases', 'db-archive', 'downloads'].map((name) => ({ name, creationDate: '' })),
}));

import AnalyticsSection from '../components/AnalyticsSection';

afterEach(() => vi.unstubAllGlobals());

test('one usage request for three buckets', async () => {
  const http = mockFetch();
  http.on('GET', /^\/_\/api\/admin\//, json({}));
  http.on('GET', '/_/api/admin/diagnostics/scan/status', json({ buckets: {}, running: {} }));
  http.on('GET', '/_/api/admin/usage/buckets', json({
    buckets: [{ bucket: 'releases', object_count: 7, logical_bytes: 700, stored_bytes: 70, savings_percentage: 90, last_scan_at: null, never_scanned: true }],
  }));
  renderWithQuery(<AnalyticsSection config={null} />);
  await waitFor(() => expect(http.callsTo('GET', '/_/api/admin/usage/buckets')).toHaveLength(1));
  await new Promise((r) => setTimeout(r, 100));
  expect(http.calls.filter((c) => c.path.startsWith('/_/api/admin/usage/bucket/'))).toEqual([]);
  expect(http.callsTo('GET', '/_/api/admin/usage/buckets')).toHaveLength(1);
});
