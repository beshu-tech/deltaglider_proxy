/**
 * AuditLogPanel reads the audit ring: rows, the empty explainer, a fetch
 * failure, the client-side filter, and the 3 s auto-refresh.
 */
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import AuditLogPanel from '../components/AuditLogPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const AUDIT = '/_/api/admin/audit';
const entry = (action: string, user: string, bucket = '') => ({
  timestamp: '2026-10-30T10:00:00Z', action, user, target: '', ip: '10.0.0.7', ua: '', bucket, path: '',
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

test('lists the entries and filters them client-side', async () => {
  http.on('GET', AUDIT, json({ entries: [entry('login', 'dana'), entry('delete_object', 'ci-uploader', 'releases')] }));
  const user = userEvent.setup();
  renderWithQuery(<AuditLogPanel />);
  // UX harvest C41: the count is stated once.
  expect(await screen.findByText('2 entries')).toBeInTheDocument();
  expect(screen.queryByText(/Showing 2/)).toBeNull();
  expect(http.callsTo('GET', AUDIT)[0].path).toBe('/_/api/admin/audit?limit=500');
  await user.type(screen.getByRole('textbox', { name: 'Filter audit entries' }), 'releases');
  expect(screen.getByText('1 of 2 shown')).toBeInTheDocument();
  expect(screen.queryByText('dana')).toBeNull();
});

test('an empty ring explains what gets recorded', async () => {
  http.on('GET', AUDIT, json({ entries: [] }));
  renderWithQuery(<AuditLogPanel />);
  expect(await screen.findByText('No audit entries yet.')).toBeInTheDocument();
});

test('a fetch failure shows an alert', async () => {
  http.on('GET', AUDIT, json({ error: 'ring unavailable' }, 500));
  renderWithQuery(<AuditLogPanel />);
  expect(await screen.findByText(/Failed to load audit entries: .*ring unavailable/)).toBeInTheDocument();
});

test('an expired session sends the user to sign in', async () => {
  http.on('GET', AUDIT, json({ error: 'unauthorized' }, 401));
  const onSessionExpired = vi.fn();
  renderWithQuery(<AuditLogPanel onSessionExpired={onSessionExpired} />);
  await waitFor(() => expect(onSessionExpired).toHaveBeenCalled());
  expect(screen.queryByText(/Failed to load audit entries/)).toBeNull();
});

test('auto-refresh polls every 3 s only while switched on', async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  http.on('GET', AUDIT, json({ entries: [entry('login', 'dana')] }));
  const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
  renderWithQuery(<AuditLogPanel />);
  await screen.findByText('1 entry');
  await act(() => vi.advanceTimersByTimeAsync(7000));
  expect(http.callsTo('GET', AUDIT)).toHaveLength(1);
  await user.click(screen.getByRole('switch', { name: 'Auto-refresh every 3 seconds' }));
  await act(() => vi.advanceTimersByTimeAsync(3100));
  await waitFor(() => expect(http.callsTo('GET', AUDIT)).toHaveLength(2));
});
