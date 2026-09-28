/**
 * Batch-D audit: with a read-only config file (Docker `:ro`, a Kubernetes
 * `subPath`) a section apply stays live but is lost at the next restart. The
 * server answers the PUT with 500 + `ok: true` + `persist_error`. The GUI
 * says what happened (not a bare "Apply failed"), and the admin page shows a
 * banner while `GET /config` reports `config_file_writable: false`.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import AdminPage from '../components/AdminPage';
import { ObjectSizeLimitCard } from '../components/advancedPanels';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const SECTION = '/_/api/admin/config/section/advanced';
afterEach(() => vi.unstubAllGlobals());

test('a PUT that applied but did not persist says the change is lost at restart', async () => {
  const http = mockFetch();
  http.on('GET', SECTION, json({ max_object_size: 100 * 1024 * 1024 }));
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [], config_file_writable: false }));
  http.on('POST', `${SECTION}/validate`, json({ ok: true, diff: { advanced: { max_object_size: { before: 104857600, after: 262144000 } } } }));
  http.on('PUT', SECTION, json({
    ok: true,
    persist_error: '/etc/deltaglider_proxy/config.yaml: Read-only file system (os error 30)',
    warnings: ['Applied section advanced in memory but FAILED to persist'],
  }, 500));
  const user = userEvent.setup();
  renderWithQuery(<ObjectSizeLimitCard />);
  const input = await screen.findByRole('spinbutton', { name: 'Maximum object size (MiB)' });
  await waitFor(() => expect(input).toHaveValue('100'));
  await user.clear(input);
  await user.type(input, '250');
  await user.click(screen.getByRole('button', { name: /Review & apply/ }));
  await user.click(await screen.findByRole('button', { name: 'Apply and persist changes' }));
  expect(
    await screen.findByText(/Applied to the running proxy, but the config file is read-only, so this change is lost at the next restart\. Export the YAML and update your deployment\./),
  ).toBeInTheDocument();
  // Applied in memory: the editor is clean again, not stuck dirty.
  await waitFor(() => expect(screen.queryByRole('button', { name: /Review & apply/ })).not.toBeInTheDocument());
});

test('the admin page shows a banner while the config file is read-only', async () => {
  const http = mockFetch();
  for (const m of ['GET', 'POST']) http.on(m, /./, json({}, 404));
  http.on('GET', '/_/api/whoami', json({
    mode: 'iam',
    user: { name: 'dana', access_key_id: 'AKDANA', is_admin: true, permissions: [] },
  }));
  http.on('GET', '/_/api/admin/session', json({ valid: true, admin_gui: true }));
  http.on('GET', '/_/api/admin/config', json({
    iam_mode: 'gui', env_overrides: [],
    config_file_writable: false, config_file_path: '/etc/deltaglider_proxy/config.yaml',
  }));
  renderWithQuery(<AdminPage onBack={() => {}} onShowShortcuts={() => {}} subPath="dashboard" canAdmin />);
  expect(await screen.findByText(/The config file \/etc\/deltaglider_proxy\/config\.yaml is read-only/)).toBeInTheDocument();
});
