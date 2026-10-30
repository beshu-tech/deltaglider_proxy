/**
 * Issue #116 P3: on several instances, a bucket whose writes this instance
 * refuses (its encryption differs from the cluster's) carries a warning
 * chip on its card, with a link to the encryption witness on the System
 * page. Other buckets carry none, and a single instance shows none.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BucketsPanel from '../components/BucketsPanel';
import { NavigationContext } from '../NavigationContext';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { eff } from '../test/encryption';

const WITNESS = '/_/api/admin/encryption-witness';
const BACKEND = {
  name: 'hetzner-fsn1', backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null,
  has_credentials: true, encryption: { mode: 'none', has_key: false, shim_active: false },
};
const CHIP = /Writes refused on this instance: encryption differs from the cluster/;

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({
    env_overrides: [], max_delta_ratio: 0.75, backends: [BACKEND], default_backend: 'hetzner-fsn1',
    bucket_policies: { releases: { backend: 'hetzner-fsn1' } },
  }));
  http.on('GET', '/_/api/admin/backends', json({ backends: [BACKEND], default_backend: 'hetzner-fsn1' }));
  http.on('GET', '/_/api/admin/buckets', json({
    buckets: [
      { name: 'releases', backend_name: 'hetzner-fsn1', effective_encryption: eff({ backend: 'hetzner-fsn1' }) },
      { name: 'downloads', backend_name: 'hetzner-fsn1', effective_encryption: eff({ backend: 'hetzner-fsn1' }) },
    ],
  }));
  http.on('GET', '/_/api/admin/config/section/storage', json({ buckets: { releases: { backend: 'hetzner-fsn1' } } }));
  http.on('GET', '/_/api/admin/config/section/advanced', json({}));
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
});
afterEach(() => vi.unstubAllGlobals());

const mismatch = {
  backend: 'hetzner-fsn1', bucket: 'releases',
  local: { mode: 'none' }, published: { mode: 'aes256-gcm-proxy', key_id: 'k1' }, set_by: 'dgp-0',
};

test('the refused bucket carries the chip, and its link opens the witness', async () => {
  http.on('GET', WITNESS, json({ status: 'mismatch', mismatches: [mismatch] }));
  const navigate = vi.fn();
  renderWithQuery(
    <NavigationContext.Provider value={{ navigate, subPath: 'storage/buckets' }}>
      <BucketsPanel />
    </NavigationContext.Provider>,
  );
  const releases = await screen.findByRole('button', { name: /^releases / });
  const chip = await screen.findByTestId('witness-refused-chip');
  expect(chip).toHaveTextContent(CHIP);
  // On the releases card, the one after its status row.
  expect(releases.parentElement).toContainElement(chip);
  expect(screen.getAllByTestId('witness-refused-chip')).toHaveLength(1);
  await userEvent.setup().click(screen.getByRole('link', { name: 'Open the encryption witness' }));
  expect(navigate).toHaveBeenCalledWith('/_/admin/system?focus=encryption-witness');
  expect(releases).toHaveAttribute('aria-expanded', 'false');
});

test('a single instance shows no chip', async () => {
  http.on('GET', WITNESS, json(null));
  renderWithQuery(<BucketsPanel />);
  await screen.findByRole('button', { name: /^releases / });
  await vi.waitFor(() => expect(http.callsTo('GET', WITNESS)).toHaveLength(1));
  expect(screen.queryByTestId('witness-refused-chip')).toBeNull();
});
