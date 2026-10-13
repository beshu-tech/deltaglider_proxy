/**
 * Jobs QA maint M6: the legacy-key check used the server default of 10,000
 * objects and told the operator to use the admin API and to expect
 * "several minutes", while a check of 20,000 objects answers in about 1 s.
 * The UI now asks for 50,000 objects and says so.
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
    backend: 'local-disk', legacy_key_id: 'k-2025', buckets: ['releases'], objects_scanned: 50000,
    objects_under_legacy_key: 0, references_scanned: 3, references_under_legacy_key: 0,
    examples: [], errors: [], complete: false, limit: 50000, safe_to_clear: false,
  }));
});
afterEach(() => vi.unstubAllGlobals());

test('the check asks for 50,000 objects and the copy says so', async () => {
  const user = userEvent.setup();
  renderWithQuery(
    <BackendEncryptionEditor backendName="local-disk" current={CURRENT} onApply={async () => {}} onClearLegacy={async () => {}} />,
  );
  expect(await screen.findByText(/reads the metadata of up to 50,000 objects/)).toBeInTheDocument();
  expect(screen.queryByText(/several minutes/)).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Check usage' }));
  await waitFor(() => expect(http.callsTo('GET', USAGE)).toHaveLength(1));
  expect(http.callsTo('GET', USAGE)[0].path).toContain('limit=50000');
  expect(await screen.findByText(/stopped at its limit of 50,000 objects/)).toBeInTheDocument();
});
