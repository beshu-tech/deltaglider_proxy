/**
 * A failed compression-health scan was only logged on the server, so every
 * poll found no result and started a new full bucket scan (one every 3 s
 * for 5 minutes). The server now answers the failure without a new scan;
 * the panel stops polling, shows the error, and its button asks for a new
 * scan (POST) instead of reading the same failure again.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import DeltaEfficiencyPanel from '../components/DeltaEfficiencyPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const GET = /^\/_\/api\/admin\/diagnostics\/delta-efficiency\?/;
const POST = '/_/api/admin/diagnostics/delta-efficiency/scan';
let http: ReturnType<typeof mockFetch>;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [{ name: 'releases' }] }));
  http.on('GET', GET, json({
    bucket: 'releases', min_deltas: 3, scanning: false, status: 'scan_failed', error: 'listing releases failed: SlowDown',
  }, 202));
  http.on('POST', POST, json({ bucket: 'releases', min_deltas: 3, scanning: true, status: 'scan_started' }, 202));
});
afterEach(() => vi.unstubAllGlobals());

test('a failed scan shows its error after one poll, and the button starts a new scan', async () => {
  renderWithQuery(<DeltaEfficiencyPanel />);
  const scan = await screen.findByRole('button', { name: /Scan/ });
  await waitFor(() => expect(scan).toBeEnabled());
  await userEvent.click(scan);
  expect(await screen.findByText('Could not load the scan result')).toBeInTheDocument();
  expect(screen.getByRole('alert')).toHaveTextContent(/listing releases failed|slow/i);
  expect(http.calls.filter((c) => c.method === 'GET' && GET.test(c.path))).toHaveLength(1);
  expect(http.callsTo('POST', POST)).toHaveLength(0);

  await userEvent.click(screen.getByRole('button', { name: /Retry/ }));
  await waitFor(() => expect(http.callsTo('POST', POST)).toHaveLength(1));
});

test('a minimum that is not a whole number from 1 to 1000 shows an error and is not clamped', async () => {
  renderWithQuery(<DeltaEfficiencyPanel />);
  const field = await screen.findByLabelText('Minimum deltas per folder');
  await userEvent.clear(field);
  await userEvent.type(field, '0');
  await userEvent.tab();
  expect(await screen.findByText('Enter a whole number between 1 and 1,000.')).toBeInTheDocument();
  expect(field).toHaveValue('0');
  expect(field).toHaveAttribute('aria-invalid', 'true');
  // Scan does not run with an invalid minimum.
  await userEvent.click(screen.getByRole('button', { name: /Scan/ }));
  await new Promise((r) => setTimeout(r, 20));
  expect(http.calls.filter((c) => c.method === 'GET' && GET.test(c.path))).toHaveLength(0);
});
