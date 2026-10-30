/**
 * BucketScanCard reads the persisted scan totals through the shared
 * bucket-scans query: totals, the idle state, a best-effort failure, the
 * 30 s poll, and a scan whose end refreshes the totals.
 */
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState, type ReactNode } from 'react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { BucketScanProgress } from '../adminApi';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  listBuckets: async () => ['releases', 'db-archive', 'downloads'].map((name) => ({ name, creationDate: '' })),
}));
// jsdom has no EventSource: capture the progress callback instead.
const sse = vi.hoisted(() => ({ emit: null as null | ((f: BucketScanProgress) => void) }));
vi.mock('../adminApi/bucketScan', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../adminApi/bucketScan')>()),
  subscribeBucketScan: (_b: string, onProgress: (f: BucketScanProgress) => void) => {
    sse.emit = onProgress;
    return () => {
      sse.emit = null;
    };
  },
}));

import BucketScanCard from '../components/BucketScanCard';

const STATUS = '/_/api/admin/diagnostics/scan/status';
const result = (bucket: string, objects: number) => ({
  bucket, total_objects: objects, total_original_bytes: 1000, total_stored_bytes: 100, savings_percentage: 90,
  started_at: '2026-10-30T09:00:00Z', completed_at: '2026-10-30T09:01:00Z', duration_ms: 60_000, version: 1,
});

function WithActions() {
  const [actions, setActions] = useState<ReactNode>(null);
  return (
    <>
      <div>{actions}</div>
      <BucketScanCard onRenderActions={setActions} />
    </>
  );
}

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

test('shows the persisted totals and the buckets not measured yet', async () => {
  http.on('GET', STATUS, json({ buckets: { releases: result('releases', 1234) }, running: {} }));
  renderWithQuery(<BucketScanCard />);
  expect(await screen.findByText('1,234')).toBeInTheDocument();
  expect(await screen.findByText(/2 of 3 buckets not measured yet/)).toBeInTheDocument();
});

test('no scan yet shows the idle call to action', async () => {
  http.on('GET', STATUS, json({ buckets: {}, running: {} }));
  renderWithQuery(<BucketScanCard />);
  expect(await screen.findByText(/No scans yet\./)).toBeInTheDocument();
  expect(await screen.findByText('3 buckets ready to scan')).toBeInTheDocument();
});

test('a failed status read is not fatal', async () => {
  http.on('GET', STATUS, json({ error: 'scan store unreadable' }, 500));
  renderWithQuery(<BucketScanCard />);
  await waitFor(() => expect(http.callsTo('GET', STATUS)).toHaveLength(1));
  expect(await screen.findByText(/No scans yet\./)).toBeInTheDocument();
  expect(screen.queryByText(/scan store unreadable/)).toBeNull();
});

test('the totals refresh every 30 s', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  let objects = 10;
  http.on('GET', STATUS, () => json({ buckets: { releases: result('releases', objects) }, running: {} }));
  renderWithQuery(<BucketScanCard />);
  await screen.findByText('10');
  objects = 20;
  await act(() => vi.advanceTimersByTimeAsync(30_100));
  expect(await screen.findByText('20')).toBeInTheDocument();
});

test('Run full scan starts the first bucket; its end refreshes the totals', async () => {
  let done = false;
  http.on('GET', STATUS, () => json({ buckets: done ? { releases: result('releases', 77) } : {}, running: {} }));
  http.on('POST', '/_/api/admin/diagnostics/scan/start', json({}));
  const user = userEvent.setup();
  renderWithQuery(<WithActions />);
  const run = await screen.findByRole('button', { name: /Run full scan/ });
  await waitFor(() => expect(run).toBeEnabled());
  await user.click(run);
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/diagnostics/scan/start')).toHaveLength(1));
  expect(http.callsTo('POST', '/_/api/admin/diagnostics/scan/start')[0].path).toContain('bucket=releases');
  await waitFor(() => expect(sse.emit).not.toBeNull());
  done = true;
  act(() => sse.emit!({
    bucket: 'releases', objects: 77, original_bytes: 1000, stored_bytes: 100, pages_done: 1, has_more: false,
    finished: true, error: null, started_at: '2026-10-30T09:00:00Z',
  }));
  await waitFor(() => expect(http.callsTo('GET', STATUS)).toHaveLength(2));
  expect(await screen.findByText('77')).toBeInTheDocument();
});

// UX harvest C52: "0 buckets ready to scan" showed before the data arrived.
test('a skeleton, not zero figures, until the scans are read', async () => {
  let release!: () => void;
  const gate = new Promise<void>((r) => (release = r));
  http.on('GET', STATUS, async () => {
    await gate;
    return json({ buckets: {}, running: {} });
  });
  renderWithQuery(<BucketScanCard />);
  expect(await screen.findByRole('status', { name: 'Loading scans' })).toBeInTheDocument();
  expect(screen.queryByText(/ready to scan/)).toBeNull();
  release();
  expect(await screen.findByText('3 buckets ready to scan')).toBeInTheDocument();
});
