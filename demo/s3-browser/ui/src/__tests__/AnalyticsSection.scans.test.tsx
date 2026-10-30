/**
 * AnalyticsSection reads the shared bucket-scans query: coverage, the idle
 * state, a best-effort failure, and the re-attach to a scan that kept
 * running on the server while the page was closed.
 */
import { screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  listBuckets: async () => ['releases', 'db-archive', 'downloads'].map((name) => ({ name, creationDate: '' })),
}));
const sse = vi.hoisted(() => ({ subscribed: [] as string[] }));
vi.mock('../adminApi/bucketScan', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../adminApi/bucketScan')>()),
  subscribeBucketScan: (bucket: string) => {
    sse.subscribed.push(bucket);
    return () => {};
  },
}));

import AnalyticsSection from '../components/AnalyticsSection';

const STATUS = '/_/api/admin/diagnostics/scan/status';
const result = (bucket: string) => ({
  bucket, total_objects: 10, total_original_bytes: 1000, total_stored_bytes: 100, savings_percentage: 90,
  started_at: '2026-10-30T09:00:00Z', completed_at: '2026-10-30T09:01:00Z', duration_ms: 60_000, version: 1,
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  sse.subscribed = [];
  http = mockFetch();
  http.on('GET', '/_/api/admin/usage/buckets', json({ buckets: [] }));
  http.on('POST', '/_/api/admin/diagnostics/scan/start', json({}));
});
afterEach(() => vi.unstubAllGlobals());

test('shows how many buckets the cached scans cover', async () => {
  http.on('GET', STATUS, json({ buckets: { releases: result('releases') }, running: {} }));
  renderWithQuery(<AnalyticsSection config={null} />);
  expect(await screen.findByText(/1 of 3 scanned/)).toBeInTheDocument();
  expect(sse.subscribed).toEqual([]);
});

test('no scan yet reads as none of the buckets scanned', async () => {
  http.on('GET', STATUS, json({ buckets: {}, running: {} }));
  renderWithQuery(<AnalyticsSection config={null} />);
  expect(await screen.findByText(/0 of 3 scanned/)).toBeInTheDocument();
});

test('a failed status read is not fatal', async () => {
  http.on('GET', STATUS, json({ error: 'scan store unreadable' }, 500));
  renderWithQuery(<AnalyticsSection config={null} />);
  await waitFor(() => expect(http.callsTo('GET', STATUS)).toHaveLength(1));
  expect(await screen.findByText(/0 of 3 scanned/)).toBeInTheDocument();
});

test('a scan still running on the server is followed again', async () => {
  http.on('GET', STATUS, json({
    buckets: {},
    running: { 'db-archive': { bucket: 'db-archive', objects: 5, original_bytes: 0, stored_bytes: 0, pages_done: 1, has_more: true, finished: false, error: null, started_at: '' } },
  }));
  renderWithQuery(<AnalyticsSection config={null} />);
  await waitFor(() => expect(sse.subscribed).toEqual(['db-archive']));
});
