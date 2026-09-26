/**
 * A created or duplicated user's generated secret is shown exactly once.
 *
 * Pins the browser-review blocker: selecting the new user writes ?user=<id>
 * into the URL, and the URL-sync effect then cleared the credentials, so the
 * secret flashed for one render and was gone. The secret now sits in a modal
 * that only the admin's explicit "I have copied it" closes.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import UsersPanel from '../components/UsersPanel';
import { NavigationContext } from '../NavigationContext';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const SECRET = 'wJalrXUtnFEMI-generated-secret-1234';

/** Feeds navigate() back into the panel's `search` prop, like the real router. */
function Host() {
  const [search, setSearch] = useState('');
  return (
    <NavigationContext.Provider
      value={{ subPath: 'access/users', navigate: (url) => setSearch(url.includes('?') ? url.slice(url.indexOf('?')) : '') }}
    >
      <UsersPanel search={search} />
    </NavigationContext.Provider>
  );
}

let http: ReturnType<typeof mockFetch>;
let users: Array<Record<string, unknown>>;

beforeEach(() => {
  users = [
    { id: 1, name: 'dana', access_key_id: 'AKDANA', enabled: true, created_at: '', permissions: [], group_ids: [] },
  ];
  http = mockFetch();
  http.on('GET', '/_/api/admin/users', () => json(users));
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui' }));
  http.on('GET', '/_/api/admin/policies', json([]));
  http.on('GET', '/_/api/admin/groups', json([]));
  http.on('POST', '/_/api/admin/users', () => {
    const created = { id: 2, name: 'ci-uploader', access_key_id: 'AKNEW', enabled: true, created_at: '', permissions: [], group_ids: [] };
    users = [...users, created];
    return json({ ...created, secret_access_key: SECRET });
  });
  http.on('POST', '/_/api/admin/users/1/clone', () => {
    const cloned = { id: 3, name: 'dana-copy', access_key_id: 'AKCLONE', enabled: true, created_at: '', permissions: [], group_ids: [] };
    users = [...users, cloned];
    return json({ ...cloned, secret_access_key: SECRET });
  });
});
afterEach(() => {
  vi.unstubAllGlobals();
});

async function expectSecretModal(user: ReturnType<typeof userEvent.setup>, ak: string) {
  const dialog = await screen.findByRole('dialog', { name: /save these credentials/i });
  expect(within(dialog).getByText(SECRET)).toBeInTheDocument();
  expect(within(dialog).getByText(ak)).toBeInTheDocument();
  // The URL now names the new user; the secret must survive that.
  await new Promise((r) => setTimeout(r, 50));
  expect(screen.getByRole('dialog', { name: /save these credentials/i })).toBeInTheDocument();
  expect(within(dialog).getByRole('button', { name: /copy secret key/i })).toBeInTheDocument();
  await user.click(within(dialog).getByRole('button', { name: /i have copied it/i }));
  await waitFor(() => expect(screen.queryByText(SECRET)).not.toBeInTheDocument());
}

test('creating a user shows the generated secret until the admin dismisses it', async () => {
  const user = userEvent.setup();
  renderWithQuery(<Host />);
  await user.click(await screen.findByRole('button', { name: /new/i }));
  await user.type(screen.getByPlaceholderText('e.g. ci-bot'), 'ci-uploader');
  await user.click(screen.getByRole('button', { name: /create user/i }));
  await expectSecretModal(user, 'AKNEW');
});

test('duplicating a user shows the fresh secret until the admin dismisses it', async () => {
  const user = userEvent.setup();
  renderWithQuery(<Host />);
  await user.click(await screen.findByTitle('Duplicate user with fresh credentials'));
  await expectSecretModal(user, 'AKCLONE');
});
