/**
 * Issue #116 P2: the Backends page reads every per-bucket fact from the
 * server's `effective_encryption`.
 *
 *   * The card says how many buckets take the backend's encryption and how
 *     many set their own.
 *   * After an encryption change of the backend, the re-encrypt proposal
 *     lists only the buckets whose effective encryption changed. A bucket
 *     with its own key is not touched by a backend change, so it is not
 *     offered (the old proposal offered every bucket on the backend).
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendsPanel from '../components/BackendsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';
import { bucketKeyEff, eff, inheritedAesEff } from '../test/encryption';

const SECTION = '/_/api/admin/config/section/storage';
const AES = { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-2026', shim_active: false };
const BACKEND = {
  name: 'hetzner-fsn1', backend_type: 's3', path: null, endpoint: 'https://fsn1.example', region: 'eu-central-1',
  force_path_style: true, has_credentials: true, encryption: AES,
};

let http: ReturnType<typeof mockFetch>;
let applied = false;
beforeEach(() => {
  applied = false;
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75 }));
  http.on('GET', '/_/api/admin/backends', json({ backends: [BACKEND], default_backend: 'hetzner-fsn1' }));
  // Before the apply: releases and downloads inherit AES; db-archive has its
  // own key. After it: the backend is plaintext, db-archive is unchanged.
  http.on('GET', '/_/api/admin/buckets', () =>
    json({
      buckets: [
        {
          name: 'releases',
          backend_name: 'hetzner-fsn1',
          effective_encryption: applied
            ? eff({ reencrypt: true, legacy_key_id: 'k-2026' })
            : inheritedAesEff('hetzner-fsn1', 'k-2026'),
        },
        {
          name: 'downloads',
          backend_name: 'hetzner-fsn1',
          effective_encryption: applied
            ? eff({ reencrypt: true, legacy_key_id: 'k-2026' })
            : inheritedAesEff('hetzner-fsn1', 'k-2026'),
        },
        { name: 'db-archive', backend_name: 'hetzner-fsn1', effective_encryption: bucketKeyEff() },
      ],
    }),
  );
  http.on('GET', SECTION, json({ backends: [{ name: 'hetzner-fsn1', type: 's3' }] }));
  http.on('PUT', SECTION, () => {
    applied = true;
    return json({ ok: true });
  });
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('the card counts the buckets that inherit and the buckets that override', async () => {
  renderWithQuery(<BackendsPanel />);
  expect(await screen.findByTestId('backend-encryption-counts')).toHaveTextContent(
    'Encryption: Default for 2 buckets · 1 bucket overrides',
  );
});

test('after a backend change, only the buckets whose encryption changed are proposed', async () => {
  const user = userEvent.setup();
  renderWithQuery(<BackendsPanel />);
  await screen.findByTestId('backend-encryption-counts');
  // Encryption mode: None (plaintext), then Apply.
  await user.click(within(screen.getByTestId('encryption-mode-select')).getByRole('combobox'));
  await user.click(await screen.findByText('None (plaintext)', { selector: '.ant-select-item-option-content' }));
  await user.click(screen.getByRole('button', { name: 'Apply encryption change' }));
  const dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent('Decrypt existing objects?');
  expect(within(dialog).getByRole('checkbox', { name: 'downloads' })).toBeChecked();
  expect(within(dialog).getByRole('checkbox', { name: 'releases' })).toBeChecked();
  expect(within(dialog).queryByRole('checkbox', { name: 'db-archive' })).toBeNull();
  await waitFor(() => expect(http.callsTo('PUT', SECTION)).toHaveLength(1));
  // Turning the backend's encryption off confirms the downgrade for the
  // buckets that inherit from it.
  expect(http.callsTo('PUT', SECTION)[0].headers['x-dgp-confirm-encryption-downgrade']).toBe('backend:hetzner-fsn1');
});

/** P3 contract: a new default backend that makes unrouted buckets
 *  plaintext is confirmed first, and only then sends `route:default`. */
test('a plaintext new default backend is confirmed and sends route:default', async () => {
  const user = userEvent.setup();
  http.on('POST', '/_/api/admin/backends', json({ success: true }));
  renderWithQuery(<BackendsPanel />);
  await user.click(await screen.findByTestId('backends-add'));
  await user.type(screen.getByTestId('backend-name'), 'local-disk');
  await user.type(screen.getByTestId('backend-path'), '/var/lib/dgp-local');
  await user.click(document.getElementById('backend-set-default')!);
  await user.click(screen.getByTestId('backend-create'));
  const dialogs = await screen.findAllByRole('dialog');
  const confirm = dialogs[dialogs.length - 1];
  expect(confirm).toHaveTextContent(/default backend hetzner-fsn1 encrypts/);
  expect(http.callsTo('POST', '/_/api/admin/backends')).toHaveLength(0);
  await user.click(within(confirm).getByRole('button', { name: 'Make it the default' }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/backends')).toHaveLength(1));
  expect(http.callsTo('POST', '/_/api/admin/backends')[0].headers['x-dgp-confirm-encryption-downgrade']).toBe('route:default');
});
