/**
 * UX harvest cycle 3 on the Jobs page:
 * - D17: a failed list was an inline box "Runs failed (500) [internal]: boom".
 * - D35: the empty list was one grey sentence.
 * - D66: a gold "Delete" kind tag beside a green "succeeded", and a full
 *   progress bar for a finished one-object job.
 */
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import type { RunEntry } from '../adminApi';
import JobsPanel from '../components/jobs/JobsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const RUNS = '/_/api/admin/jobs/runs';
let http: ReturnType<typeof mockFetch>;

const done: RunEntry = {
  id: 'maintenance:9',
  job_id: 'maintenance:9',
  origin: 'admin',
  kind: 'delete',
  name: 'releases',
  scope: { bucket: 'releases' },
  trigger: 'oneoff',
  status: 'completed',
  status_raw: 'completed',
  progress: { processed: 1, bytes: 10, failed: 0, skipped: 0 },
  detail: {},
} as RunEntry;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('a failed list is one plain error with Retry', async () => {
  http.on('GET', RUNS, json({ error: 'boom', code: 'internal' }, 500));
  renderWithQuery(<JobsPanel />);
  expect(await screen.findByText('Could not load the runs')).toBeInTheDocument();
  expect(screen.queryByText(/boom|\(500\)/)).toBeNull();
  http.on('GET', RUNS, json({ runs: [] }));
  await userEvent.setup().click(screen.getByRole('button', { name: /Retry/ }));
  expect(await screen.findByText('No runs yet')).toBeInTheDocument();
});

test('a finished run: a neutral kind tag, and the outcome without a bar', async () => {
  http.on('GET', RUNS, json({ runs: [done] }));
  renderWithQuery(<JobsPanel />);
  await screen.findAllByText('releases');
  const row = screen.getAllByRole('row')[1];
  const kindTag = within(row).getAllByText(/Delete/i).find((el) => el.classList.contains('ant-tag'));
  expect(kindTag).toBeDefined();
  expect(kindTag?.className).not.toMatch(/ant-tag-(gold|blue|purple)/);
  const meter = row.querySelector('.dg-meter') as HTMLElement;
  expect(meter).toHaveAttribute('data-bar', 'off');
  expect(meter.querySelector('.dg-meter-track')).toBeNull();
});
