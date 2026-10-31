/**
 * UX harvest D16/D17 on the Backends page: a failed load showed a red box
 * with wire text above a "Storage backends — No backend configured" card.
 * Now one ErrorState with Retry, and no card that states placeholder facts.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendsPanel from '../components/BackendsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75 }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', '/_/api/admin/config/section/storage', json({}));
});
afterEach(() => vi.unstubAllGlobals());

test('a failed load is one plain error with Retry, without the backends card', async () => {
  http.on('GET', '/_/api/admin/backends', json({ error: 'boom', code: 'internal' }, 500));
  renderWithQuery(<BackendsPanel />);
  expect(await screen.findByText('Could not load the backends')).toBeInTheDocument();
  expect(screen.queryByText('No backend configured.')).toBeNull();
  expect(screen.queryByText(/boom/)).toBeNull();
  http.on('GET', '/_/api/admin/backends', json({ backends: [], default_backend: null }));
  await userEvent.setup().click(screen.getByRole('button', { name: /Retry/ }));
  expect(await screen.findByText('No backend configured.')).toBeInTheDocument();
});
