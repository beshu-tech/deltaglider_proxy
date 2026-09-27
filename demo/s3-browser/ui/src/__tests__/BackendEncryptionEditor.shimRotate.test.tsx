// review4 frontend-5: while the legacy (decrypt-only) slot holds a key, the
// server refuses a rotation or a mode change (the current key has nowhere
// to go). The editor must not offer them, and must say why.
import { screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  mockFetch().on('GET', '/_/api/admin/backends/local-disk/legacy-key-usage', json({
    backend: 'local-disk', legacy_key_id: 'k-2025', buckets: [], objects_scanned: 0,
    objects_under_legacy_key: 0, references_scanned: 0, references_under_legacy_key: 0,
    examples: [], errors: [], complete: true, limit: 10000, safe_to_clear: false,
  }));
});
afterEach(() => vi.unstubAllGlobals());

function renderWith(shim_active: boolean) {
  renderWithQuery(
    <BackendEncryptionEditor
      backendName="local-disk"
      current={{ mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-2026', shim_active }}
      onApply={async () => {}}
      onClearLegacy={async () => {}}
    />,
  );
}

test('with a legacy key held, Rotate and the mode select are disabled with the reason', async () => {
  renderWith(true);
  const rotate = await screen.findByRole('button', { name: /Rotate key/ });
  expect(rotate).toBeDisabled();
  expect(rotate).toHaveAttribute('title', expect.stringMatching(/legacy key/));
  expect(screen.getByRole('combobox', { name: 'Encryption mode' })).toBeDisabled();
  expect(screen.getByText(/Clear the legacy key before you change the key or the mode/)).toBeInTheDocument();
});

test('without a legacy key, Rotate and the mode select are enabled', async () => {
  renderWith(false);
  expect(await screen.findByRole('button', { name: /Rotate key/ })).toBeEnabled();
  expect(screen.getByRole('combobox', { name: 'Encryption mode' })).toBeEnabled();
});
