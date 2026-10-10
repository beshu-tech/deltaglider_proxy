/**
 * Cockroach scan 2026-10-10, UI finding F: the decrypt-only shim banner
 * fetched `GET /backends/:name/legacy-key-usage` on mount. That request HEADs
 * every object of the backend (two backend HEADs each, plus two per delta
 * reference, one at a time), so opening Storage → Backends during a key
 * rotation kept a slow backend busy for minutes, again after each 5 min of
 * cache. The check now runs only when the operator clicks "Check usage".
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendEncryptionEditor from '../components/BackendEncryptionEditor';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const CURRENT = { mode: 'aes256-gcm-proxy' as const, has_key: true, key_id: 'k-2026', shim_active: true };
const USAGE = '/_/api/admin/backends/local-disk/legacy-key-usage';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', USAGE, json({
    backend: 'local-disk', legacy_key_id: 'k-2025', buckets: ['releases'], objects_scanned: 120,
    objects_under_legacy_key: 0, references_scanned: 3, references_under_legacy_key: 0,
    examples: [], errors: [], complete: true, limit: 10000, safe_to_clear: true,
  }));
});
afterEach(() => vi.unstubAllGlobals());

test('the banner does not scan on mount; "Check usage" runs the check once', async () => {
  const user = userEvent.setup();
  renderWithQuery(
    <BackendEncryptionEditor backendName="local-disk" current={CURRENT} onApply={async () => {}} onClearLegacy={async () => {}} />,
  );
  expect(await screen.findByText('Decrypt-only shim active')).toBeInTheDocument();
  // Give a mount-time query every chance to start.
  await new Promise((r) => setTimeout(r, 50));
  expect(http.callsTo('GET', USAGE)).toHaveLength(0);
  const check = screen.getByRole('button', { name: 'Check usage' });
  // Without a check, nothing proves that the legacy key is unused.
  expect(screen.getByRole('button', { name: 'Clear legacy key' })).toBeDisabled();

  await user.click(check);
  expect(await screen.findByText(/No object uses the legacy key id k-2025/)).toBeInTheDocument();
  expect(http.callsTo('GET', USAGE)).toHaveLength(1);
  await waitFor(() => expect(screen.getByRole('button', { name: 'Clear legacy key' })).toBeEnabled());

  await user.click(screen.getByRole('button', { name: 'Check again' }));
  await waitFor(() => expect(http.callsTo('GET', USAGE)).toHaveLength(2));
});
