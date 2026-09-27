// review4 §3: BackendsPanel's one-shot storage PUTs (encryption change,
// clear legacy key) go through applySection: sibling editors follow the
// version they move, and a 409 reads as a conflict, not a generic failure.
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import BackendsPanel from '../components/BackendsPanel';
import { onSectionVersionAdvanced } from '../sectionVersionBus';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/storage';
const BACKEND = {
  name: 'local-disk', backend_type: 'filesystem', path: '/var/lib/dgp', endpoint: null, region: null,
  force_path_style: null, has_credentials: false,
  encryption: { mode: 'aes256-gcm-proxy', has_key: true, key_id: 'k-2026', shim_active: true },
};

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], max_delta_ratio: 0.75 }));
  http.on('GET', '/_/api/admin/backends', json({ backends: [BACKEND], default_backend: 'local-disk' }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', '/_/api/admin/backends/local-disk/legacy-key-usage', json({
    backend: 'local-disk', legacy_key_id: 'k-2025', buckets: [], objects_scanned: 0,
    objects_under_legacy_key: 0, references_scanned: 0, references_under_legacy_key: 0,
    examples: [], errors: [], complete: true, limit: 10000, safe_to_clear: true,
  }));
  http.on('GET', SECTION, () => new Response(JSON.stringify({ backends: [{ name: 'local-disk', type: 'filesystem' }] }), {
    status: 200, headers: { 'content-type': 'application/json', etag: '"v1"' },
  }));
});
afterEach(() => vi.unstubAllGlobals());

async function clearLegacy() {
  const user = userEvent.setup();
  renderWithQuery(<BackendsPanel />);
  const button = await screen.findByRole('button', { name: 'Clear legacy key' });
  await waitFor(() => expect(button).toBeEnabled());
  await user.click(button);
  await user.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Clear' }));
}

test('clearing the legacy key moves sibling editors to the new version', async () => {
  http.on('PUT', SECTION, () => new Response(JSON.stringify({ ok: true }), {
    status: 200, headers: { 'content-type': 'application/json', etag: '"v2"' },
  }));
  const moves: Array<[string, string]> = [];
  const off = onSectionVersionAdvanced('storage', (from, to) => moves.push([from, to]));
  await clearLegacy();
  expect(await screen.findByText(/Legacy key cleared on backend 'local-disk'/)).toBeInTheDocument();
  off();
  expect(http.callsTo('PUT', SECTION)[0].headers['if-match']).toBe('"v1"');
  expect(moves).toEqual([['"v1"', '"v2"']]);
});

test('a 409 on clear legacy key reads as a conflict', async () => {
  http.on('PUT', SECTION, json({ error: 'stale' }, 409));
  await clearLegacy();
  expect(await screen.findByText(/changed in another tab or by another admin/)).toBeInTheDocument();
});
