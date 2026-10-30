/**
 * LogsPanel: the backlog query (load, empty, error, filter refetch) and the
 * live tail on top of it. A refetched backlog already holds the tail's lines,
 * so they are not counted twice.
 */
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { LogEntry } from '../adminApi';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

// jsdom has no EventSource: capture the tail's entry callback instead.
const tail = vi.hoisted(() => ({ push: null as null | ((e: LogEntry) => void) }));
vi.mock('../adminApi', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../adminApi')>()),
  streamLogs: (_f: unknown, onEntry: (e: LogEntry) => void) => {
    tail.push = onEntry;
    return () => {
      tail.push = null;
    };
  },
}));

import LogsPanel from '../components/LogsPanel';

const LOGS = '/_/api/admin/logs';
const line = (message: string): LogEntry => ({
  ts: '2026-10-30T10:00:00Z', level: 'INFO', target: 'deltaglider_proxy::api', message, fields: {},
});

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
});
afterEach(() => vi.unstubAllGlobals());

test('loads the backlog and refetches it on a level change', async () => {
  http.on('GET', LOGS, json({ entries: [line('a'), line('b')], limit: 2000 }));
  const user = userEvent.setup();
  renderWithQuery(<LogsPanel />);
  expect(await screen.findByText('2 lines')).toBeInTheDocument();
  expect(http.callsTo('GET', LOGS)[0].path).toBe(`${LOGS}?limit=2000`);
  await user.click(screen.getByRole('combobox', { name: 'Minimum level' }));
  await user.click(await screen.findByText('Warn+'));
  await waitFor(() => expect(http.callsTo('GET', LOGS)).toHaveLength(2));
  expect(http.callsTo('GET', LOGS)[1].path).toBe(`${LOGS}?level=warn&limit=2000`);
});

test('an empty ring reads as zero lines', async () => {
  http.on('GET', LOGS, json({ entries: [], limit: 2000 }));
  renderWithQuery(<LogsPanel />);
  expect(await screen.findByText('0 lines')).toBeInTheDocument();
});

test('a backlog failure is shown', async () => {
  http.on('GET', LOGS, json({ error: 'log ring disabled' }, 500));
  renderWithQuery(<LogsPanel />);
  expect(await screen.findByText(/log ring disabled/)).toBeInTheDocument();
});

test('an expired session sends the user to sign in', async () => {
  http.on('GET', LOGS, json({ error: 'unauthorized' }, 401));
  const onSessionExpired = vi.fn();
  renderWithQuery(<LogsPanel onSessionExpired={onSessionExpired} />);
  await waitFor(() => expect(onSessionExpired).toHaveBeenCalled());
});

test('tail lines prepend; a refetched backlog replaces them', async () => {
  let backlog = [line('a')];
  http.on('GET', LOGS, () => json({ entries: backlog, limit: 2000 }));
  const user = userEvent.setup();
  renderWithQuery(<LogsPanel />);
  await screen.findByText('1 lines');
  await user.click(screen.getByRole('switch'));
  await waitFor(() => expect(tail.push).not.toBeNull());
  act(() => tail.push!(line('live')));
  expect(screen.getByText('2 lines')).toBeInTheDocument();

  backlog = [line('b'), line('live'), line('a')];
  await user.click(screen.getByRole('switch'));
  await user.click(screen.getByRole('button', { name: /Refresh/ }));
  // 3, not 4: the tail line is in the new backlog.
  expect(await screen.findByText('3 lines')).toBeInTheDocument();
});
