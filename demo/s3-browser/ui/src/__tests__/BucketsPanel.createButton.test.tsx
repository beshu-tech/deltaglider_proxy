/**
 * The page's one "Create bucket" action sits in the header row, before
 * the bucket list. After the list, it followed an expanded card's form
 * and read as that card's save button.
 */
import { screen } from '@testing-library/react';
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
    buckets: [
      { name: 'releases', backend_name: 'hetzner-fsn1', effective_encryption: eff({ backend: 'hetzner-fsn1' }) },
      { name: 'downloads', backend_name: 'hetzner-fsn1', effective_encryption: eff({ backend: 'hetzner-fsn1' }) },
    ],
  }));
  http.on('GET', '/_/api/admin/config/section/storage', json({ buckets: {} }));
  http.on('GET', '/_/api/admin/config/section/advanced', json({}));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', '/_/api/admin/encryption-witness', json(null));
});
afterEach(() => vi.unstubAllGlobals());

test('Create bucket comes before every bucket card, also with the last card expanded', async () => {
  renderWithQuery(<BucketsPanel />);
  const last = await screen.findByRole('button', { name: /^releases — click to edit/ });
  await userEvent.setup().click(last);
  expect(last).toHaveAttribute('aria-expanded', 'true');
  const create = screen.getByTestId('buckets-create');
  for (const card of screen.getAllByRole('button', { name: / — click to (edit|collapse)/ })) {
    expect(create.compareDocumentPosition(card) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  }
});

test('?bucket= opens that bucket card', async () => {
  renderWithQuery(<BucketsPanel search="?bucket=releases" />);
  const card = await screen.findByRole('button', { name: /^releases — click to collapse/ });
  expect(card).toHaveAttribute('aria-expanded', 'true');
  expect(screen.getByRole('button', { name: /^downloads — click to edit/ })).toHaveAttribute('aria-expanded', 'false');
});
