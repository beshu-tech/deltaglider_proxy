/**
 * MetricsPage scrapes /_/metrics through a query: the parsed values, a
 * failed scrape, the 5 s cadence, and the manual refresh.
 */
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({ listBuckets: async () => [] }));

import MetricsPage from '../components/MetricsPage';

const SCRAPE = [
  'deltaglider_build_info{version="9.9.9",backend_type="filesystem"} 1',
  'deltaglider_cache_entries 42',
  '',
].join('\n');

let http: ReturnType<typeof mockFetch>;
const scrapes = () => http.callsTo('GET', '/_/metrics');
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
  http.on('GET', '/_/api/admin/diagnostics/scan/status', json({ buckets: {}, running: {} }));
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const text = (body: string, status = 200) => new Response(body, { status, headers: { 'content-type': 'text/plain' } });

test('renders the scraped build info', async () => {
  http.on('GET', '/_/metrics', text(SCRAPE));
  renderWithQuery(<MetricsPage />);
  expect(await screen.findByText(/v9\.9\.9 · filesystem backend/)).toBeInTheDocument();
});

test('a failed scrape is ONE error state with Retry, and no tiles that state zeros (UX harvest D17)', async () => {
  http.on('GET', '/_/metrics', text('', 500));
  renderWithQuery(<MetricsPage />);
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('Could not load the metrics');
  expect(alert).not.toHaveTextContent('500');
  expect(screen.getByRole('button', { name: /Retry/ })).toBeInTheDocument();
  expect(screen.queryByText('Cache disabled')).toBeNull();
});

test('scrapes every 5 s by default, and on Refresh now', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  http.on('GET', '/_/metrics', text(SCRAPE));
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  renderWithQuery(<MetricsPage />);
  await screen.findByText(/v9\.9\.9/);
  expect(scrapes()).toHaveLength(1);
  // The time series need two scrapes; both charts say so in the same words.
  expect(screen.getAllByText('Waiting for data')).toHaveLength(2);
  await act(() => vi.advanceTimersByTimeAsync(5_100));
  await waitFor(() => expect(scrapes()).toHaveLength(2));
  await waitFor(() => expect(screen.queryAllByText('Waiting for data')).toHaveLength(0));
  await user.click(screen.getByRole('button', { name: 'Refresh now' }));
  await waitFor(() => expect(scrapes()).toHaveLength(3));
});

// UX harvest C34: "1 hits · 0 misses" and a "100.0 %" hit rate next to
// "Too few lookups to judge the cache".
test('too few cache lookups show no hit rate, in correct plurals', async () => {
  http.on('GET', '/_/metrics', text([
    'deltaglider_cache_hits_total 1',
    'deltaglider_cache_misses_total 0',
    '',
  ].join('\n')));
  renderWithQuery(<MetricsPage />);
  expect(await screen.findByText('1 hit · 0 misses')).toBeInTheDocument();
  expect(screen.getByText('Too few lookups to judge the cache yet')).toBeInTheDocument();
  expect(screen.queryByText('100.0%')).toBeNull();
});

// UX harvest D42: charts with only zero values drew axes and labels and no bars.
test('a chart whose values are all zero shows a placeholder', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  http.on('GET', '/_/metrics', text([
    '# HELP deltaglider_delta_decisions_total decisions',
    '# TYPE deltaglider_delta_decisions_total counter',
    'deltaglider_delta_decisions_total{decision="delta"} 0',
    'deltaglider_delta_decisions_total{decision="passthrough"} 0',
    '',
  ].join('\n')));
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  renderWithQuery(<MetricsPage />);
  await act(() => vi.advanceTimersByTimeAsync(5_100));
  await waitFor(() => expect(scrapes()).toHaveLength(2));
  expect(await screen.findByText('No requests in this window')).toBeInTheDocument();
  expect(screen.getByText('No cache lookups in this window')).toBeInTheDocument();
  await user.click(screen.getByText('Detailed telemetry'));
  expect(await screen.findByText('No storage decisions yet')).toBeInTheDocument();
  expect(screen.queryByText('passthrough')).toBeNull();
});
