/**
 * In `iam_mode: declarative` the YAML config owns IAM, and the server refuses
 * `declarative-iam-apply` (403 iam_declarative). The System page's
 * "Import full IAM (YAML)" is then disabled, and a line says why (D72: the
 * import and export moved from the account menu to the System page).
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
  http.on('GET', '/_/api/admin/config/section/advanced', json({}));
}

function mount() {
  renderWithQuery(
    <AdminPage
      onBack={() => {}}
      onShowShortcuts={() => {}}
      subPath="system"
      canAdmin
      accountMenu={<AccountMenu identityLabel="dana" />}
    />,
  );
  return screen.findByRole('button', { name: /Import full IAM \(YAML\)/ });
}

test('declarative mode disables "Import full IAM (YAML)" and says why', async () => {
  setup('declarative');
  const button = await mount();
  await waitFor(() => expect(button).toBeDisabled());
  expect(screen.getByTestId('iam-import-blocked')).toHaveTextContent(/declarative.*edit the YAML config/i);
});

test('gui mode keeps "Import full IAM (YAML)" enabled', async () => {
  setup('gui');
  const button = await mount();
  expect(button).toBeEnabled();
  expect(screen.queryByTestId('iam-import-blocked')).toBeNull();
});

test('the account menu holds no import or export', async () => {
  setup('gui');
  await mount();
  await userEvent.setup().click(screen.getByRole('button', { name: /Account menu: dana/ }));
  await screen.findByRole('menu', { name: 'Account' });
  expect(screen.queryByRole('menuitem', { name: /YAML|IAM/ })).toBeNull();
});
