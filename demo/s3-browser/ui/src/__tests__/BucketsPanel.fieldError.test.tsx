/**
 * UX harvest C20 + C5 on the Buckets page: a quota the field cannot stage
 * ("2x") makes the dirty bar say "1 issue to fix" and refuse to apply; the
 * bar sits inside the bucket list's card (a viewport-fixed bar covered the
 * title of the next card).
 */
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BucketsPanel from '../components/BucketsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { eff } from '../test/encryption';

const BACKEND = {
  name: 'hetzner-fsn1', backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null,
  has_credentials: true, encryption: { mode: 'none', has_key: false, shim_active: false },
};

beforeEach(() => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({
    env_overrides: [], max_delta_ratio: 0.75, backends: [BACKEND], default_backend: 'hetzner-fsn1',
    bucket_policies: {},
  }));
  http.on('GET', '/_/api/admin/backends', json({ backends: [BACKEND], default_backend: 'hetzner-fsn1' }));
  http.on('GET', '/_/api/admin/buckets', json({
    buckets: [{ name: 'db-archive', backend_name: 'hetzner-fsn1', effective_encryption: eff({ backend: 'hetzner-fsn1' }) }],
  }));
  http.on('GET', '/_/api/admin/config/section/storage', json({ buckets: {} }));
  http.on('GET', '/_/api/admin/config/section/advanced', json({}));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', '/_/api/admin/encryption-witness', json(null));
});
afterEach(() => vi.unstubAllGlobals());

test('an invalid quota blocks Review & apply; the bar lives in the bucket card', async () => {
  const user = userEvent.setup();
  renderWithQuery(<BucketsPanel />);
  await user.click(await screen.findByRole('button', { name: /^db-archive / }));
  await user.click(screen.getByText(/^Advanced/));
  const quota = await screen.findByRole('textbox', { name: 'Quota' });
  await user.type(quota, '2');
  const apply = await screen.findByRole('button', { name: 'Review & apply' });
  expect(apply).toBeEnabled();
  await user.type(quota, 'x');
  expect(await screen.findByText('1 issue to fix')).toBeInTheDocument();
  expect(apply).toBeDisabled();
  // Inside the card that holds the bucket list, not after the next card.
  // button → Space.Compact → header row → the card.
  const listCard = screen.getByTestId('buckets-create').parentElement!.parentElement!.parentElement!;
  expect(listCard).toContainElement(document.querySelector('[data-bucket-card="db-archive"]') as HTMLElement);
  expect(within(listCard).getByRole('button', { name: 'Review & apply' })).toBe(apply);
  expect(screen.queryByTestId('dirty-bar-spacer')).toBeNull();
  // Discard drops the staged quota and the bad text.
  await user.click(screen.getByRole('button', { name: 'Discard' }));
  expect(screen.queryByText('1 issue to fix')).not.toBeInTheDocument();
});
