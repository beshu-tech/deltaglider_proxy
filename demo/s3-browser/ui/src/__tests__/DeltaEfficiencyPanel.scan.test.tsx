/**
 * The compression-health scan as a query: a 202 polls every 3 s until the
 * report lands, the report belongs to its (bucket, min deltas), and a
 * re-scan POSTs a new scan and polls it from scratch.
 */
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import DeltaEfficiencyPanel from '../components/DeltaEfficiencyPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const GET = /^\/_\/api\/admin\/diagnostics\/delta-efficiency\?/;
const POST = '/_/api/admin/diagnostics/delta-efficiency/scan';
const RUNNING = { bucket: 'releases', min_deltas: 3, scanning: true, status: 'scan_started' };
const report = (bucket: string, prefix: string, efficiency: string) => ({
  bucket, prefix, deltas: 5, passthrough: 0, reference_bytes: 1000, total_delta_bytes: 4000,
  total_original_bytes: 5000, median_delta_bytes: 800, max_delta_bytes: 900, savings_bytes: 1000,
  efficiency, ratio_median: 0.8, original_size_estimated: false, explanation: 'reference is stale',
});
const result = (bucket: string, reports: unknown[]) => ({
  bucket, scanned_deltaspaces: 4, reported_deltaspaces: reports.length, min_deltas: 3, reports,
  computed_at: '2026-10-30T10:00:00Z', cached: false,
});

let http: ReturnType<typeof mockFetch>;
const gets = () => http.calls.filter((c) => c.method === 'GET' && GET.test(c.path));
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [{ name: 'releases' }, { name: 'db-archive' }] }));
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

async function clickScan(user: ReturnType<typeof userEvent.setup>) {
  const scan = await screen.findByRole('button', { name: /Scan/ });
  await waitFor(() => expect(scan).toBeEnabled());
  await user.click(scan);
}

test('nothing is fetched until Scan is clicked', async () => {
  renderWithQuery(<DeltaEfficiencyPanel />);
  await screen.findByRole('button', { name: /Scan/ });
  await new Promise((r) => setTimeout(r, 20));
  expect(gets()).toHaveLength(0);
});

test('a 202 polls every 3 s until the report lands', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  let polls = 0;
  http.on('GET', GET, () => (++polls < 3
    ? json(RUNNING, 202)
    : json(result('db-archive', [report('db-archive', 'nightly/', 'poor')]))));
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  renderWithQuery(<DeltaEfficiencyPanel />);
  await clickScan(user);
  expect(await screen.findByText('Scan running on the server, polling…')).toBeInTheDocument();
  expect(gets()[0].path).toContain('bucket=db-archive');
  await act(() => vi.advanceTimersByTimeAsync(3100));
  await waitFor(() => expect(gets()).toHaveLength(2));
  await act(() => vi.advanceTimersByTimeAsync(3100));
  expect(await screen.findByText('1 of 1 folder needs a new reference')).toBeInTheDocument();
  expect(screen.queryByText('Scan running on the server, polling…')).toBeNull();
  // The report ends the polling.
  await act(() => vi.advanceTimersByTimeAsync(10_000));
  expect(gets()).toHaveLength(3);
});

test('an empty report says no scope met the threshold', async () => {
  http.on('GET', GET, json(result('db-archive', [])));
  const user = userEvent.setup();
  renderWithQuery(<DeltaEfficiencyPanel />);
  await clickScan(user);
  expect(await screen.findByText('No folder to report')).toBeInTheDocument();
});

test('a changed bucket drops the report', async () => {
  http.on('GET', GET, json(result('db-archive', [])));
  const user = userEvent.setup();
  renderWithQuery(<DeltaEfficiencyPanel />);
  await clickScan(user);
  await screen.findByText('No folder to report');
  await user.click(screen.getByRole('combobox', { name: 'Bucket' }));
  await user.click(await screen.findByTitle('releases'));
  await waitFor(() => expect(screen.queryByText('No folder to report')).toBeNull());
  expect(gets()).toHaveLength(1);
});

test('Re-scan POSTs a new scan, then reads its result', async () => {
  let rescanned = false;
  http.on('GET', GET, () => json(result('db-archive', rescanned ? [report('db-archive', 'nightly/', 'poor')] : [])));
  http.on('POST', POST, () => {
    rescanned = true;
    return json(RUNNING, 202);
  });
  const user = userEvent.setup();
  renderWithQuery(<DeltaEfficiencyPanel />);
  await clickScan(user);
  await screen.findByText('No folder to report');
  await user.click(screen.getByRole('button', { name: /Re-scan/ }));
  await waitFor(() => expect(http.callsTo('POST', POST)).toHaveLength(1));
  expect(http.callsTo('POST', POST)[0].body).toEqual({ bucket: 'db-archive', min_deltas: 3 });
  expect(await screen.findByText('1 of 1 folder needs a new reference')).toBeInTheDocument();
  expect(gets()).toHaveLength(2);
});

test('an expired session sends the user to sign in', async () => {
  http.on('GET', GET, json({ error: 'unauthorized' }, 401));
  const onSessionExpired = vi.fn();
  const user = userEvent.setup();
  renderWithQuery(<DeltaEfficiencyPanel onSessionExpired={onSessionExpired} />);
  await clickScan(user);
  await waitFor(() => expect(onSessionExpired).toHaveBeenCalled());
});
