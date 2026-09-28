/**
 * In `iam_mode: declarative` the YAML config owns IAM, and the server refuses
 * `declarative-iam-apply` (403 iam_declarative). The account menu's
 * "Import full IAM (YAML)" is then disabled, with a title that says why.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import AdminPage from '../components/AdminPage';
import AccountMenu from '../components/AccountMenu';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

afterEach(() => vi.unstubAllGlobals());

function setup(iamMode: 'gui' | 'declarative') {
  const http = mockFetch();
  for (const m of ['GET', 'POST']) http.on(m, /./, json({}, 404));
  http.on('GET', '/_/api/whoami', json({
    mode: 'iam',
    user: { name: 'dana', access_key_id: 'AKDANA', is_admin: true, permissions: [] },
  }));
  http.on('GET', '/_/api/admin/session', json({ valid: true, admin_gui: true }));
  http.on('GET', '/_/api/admin/config', json({ iam_mode: iamMode, env_overrides: [] }));
}

async function openMenu() {
  renderWithQuery(
    <AdminPage
      onBack={() => {}}
      onShowShortcuts={() => {}}
      subPath="dashboard"
      canAdmin
      accountMenu={<AccountMenu identityLabel="dana" canAdmin />}
    />,
  );
  const user = userEvent.setup();
  await user.click(await screen.findByRole('button', { name: /dana/ }));
  return screen.findByRole('menuitem', { name: /Import full IAM \(YAML\)/ });
}

test('declarative mode disables "Import full IAM (YAML)" and says why', async () => {
  setup('declarative');
  const item = await openMenu();
  await waitFor(() => expect(item).toBeDisabled());
  expect(item).toHaveAttribute('title', expect.stringMatching(/declarative.*edit the YAML config/i));
});

test('gui mode keeps "Import full IAM (YAML)" enabled', async () => {
  setup('gui');
  const item = await openMenu();
  expect(item).toBeEnabled();
});
