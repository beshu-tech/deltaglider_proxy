/**
 * LogsPanel: a long line is cut with an ellipsis in the list; a click (or
 * Enter) opens it — the line wraps and each structured field shows on its
 * own line — and a second click closes it.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { LogEntry } from '../adminApi';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import LogsPanel from '../components/LogsPanel';

const LOGS = '/_/api/admin/logs';
const entry: LogEntry = {
  ts: '2026-10-30T10:00:00Z',
  level: 'WARN',
  target: 'deltaglider_proxy::api',
  message: 'a very long message that the list cuts with an ellipsis',
  fields: { bucket: 'releases', key: 'builds/v1.zip' },
};

// jsdom has no layout: give the scroll box and each row a size so the
// virtualizer renders rows.
beforeEach(() => {
  vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockReturnValue(520);
  vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(800);
  vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue({
    x: 0, y: 0, top: 0, left: 0, bottom: 30, right: 800, width: 800, height: 30, toJSON: () => ({}),
  } as DOMRect);
  mockFetch().on('GET', LOGS, json({ entries: [entry], limit: 2000 }));
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

test('a click opens a line: it wraps and lists its fields; a second click closes it', async () => {
  const user = userEvent.setup();
  renderWithQuery(<LogsPanel />);
  const row = await screen.findByRole('button', { name: /a very long message/ });
  expect(row).toHaveAttribute('aria-expanded', 'false');
  expect(row).toHaveStyle({ whiteSpace: 'nowrap' });

  await user.click(row);
  expect(row).toHaveAttribute('aria-expanded', 'true');
  expect(row).toHaveStyle({ whiteSpace: 'normal' });
  expect(screen.getByText('bucket=releases')).toBeInTheDocument();
  expect(screen.getByText('key=builds/v1.zip')).toBeInTheDocument();

  await user.click(row);
  expect(row).toHaveAttribute('aria-expanded', 'false');
  expect(screen.queryByText('bucket=releases')).not.toBeInTheDocument();
});

test('Enter opens a line from the keyboard', async () => {
  const user = userEvent.setup();
  renderWithQuery(<LogsPanel />);
  const row = await screen.findByRole('button', { name: /a very long message/ });
  row.focus();
  await user.keyboard('{Enter}');
  expect(row).toHaveAttribute('aria-expanded', 'true');
});

// UX harvest D61: a cut line had no visible way to read its end.
test('"Wrap lines" shows every line in full', async () => {
  const user = userEvent.setup();
  renderWithQuery(<LogsPanel />);
  const row = await screen.findByRole('button', { name: /a very long message/ });
  expect(row).toHaveStyle({ whiteSpace: 'nowrap' });
  await user.click(screen.getByRole('switch', { name: 'Wrap lines' }));
  expect(row).toHaveStyle({ whiteSpace: 'normal' });
  expect(row).toHaveAttribute('aria-expanded', 'true');
  expect(screen.getByText('bucket=releases')).toBeInTheDocument();
});
