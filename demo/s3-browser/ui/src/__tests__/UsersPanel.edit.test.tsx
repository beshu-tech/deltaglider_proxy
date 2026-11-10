/**
 * F23 (epic #146): saving a user edit is ONE request. A rename and a new
 * access key used to go as `PUT /users/:id` and then `POST …/rotate-keys`;
 * the second one could be refused (the key belongs to another user) after the
 * first had committed, and the form showed only the error while the rename
 * was live. Now both go in the one `PUT`; a refusal applies nothing.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import UsersPanel from '../components/UsersPanel';
import { NavigationContext } from '../NavigationContext';
import { json, mockFetch, type RecordedRequest } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

function Host() {
  const [search, setSearch] = useState('?user=1');
  return (
    <NavigationContext.Provider
      value={{ subPath: 'access/users', navigate: (url) => setSearch(url.includes('?') ? url.slice(url.indexOf('?')) : '') }}
    >
      <UsersPanel search={search} />
    </NavigationContext.Provider>
  );
}

const DANA = { id: 1, name: 'dana', access_key_id: 'AKDANA', enabled: true, created_at: '', permissions: [], group_ids: [] };
let http: ReturnType<typeof mockFetch>;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/users', json([DANA]));
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui' }));
  http.on('GET', '/_/api/admin/policies', json([]));
  http.on('GET', '/_/api/admin/groups', json([]));
  http.on('POST', '/_/api/admin/users/1/rotate-keys', () => {
    throw new Error('the user edit must not rotate the keys with a second request');
  });
});
afterEach(() => vi.unstubAllGlobals());

async function renameAndRekey(newName: string, newKey: string) {
  const user = userEvent.setup();
  renderWithQuery(<Host />);
  const name = await screen.findByDisplayValue('dana');
  await user.clear(name);
  await user.type(name, newName);
  const key = screen.getByPlaceholderText('AKDANA');
  await user.clear(key);
  await user.type(key, newKey);
  await user.click(screen.getByRole('button', { name: 'Save user' }));
  // New keys ask first: the new secret is shown only once.
  await user.click(await screen.findByRole('button', { name: 'Update credentials' }));
}

test('a rename with a new access key is one PUT carrying both', async () => {
  http.on('PUT', '/_/api/admin/users/1', (req: RecordedRequest) =>
    json({ ...DANA, name: (req.body as { name: string }).name, access_key_id: 'AKNEWKEY001', secret_access_key: 'fresh-secret' }),
  );
  await renameAndRekey('dana2', 'AKNEWKEY001');
  await waitFor(() => expect(http.callsTo('PUT', '/_/api/admin/users/1')).toHaveLength(1));
  expect(http.callsTo('PUT', '/_/api/admin/users/1')[0].body).toMatchObject({ name: 'dana2', access_key_id: 'AKNEWKEY001' });
  expect(http.callsTo('POST', '/_/api/admin/users/1/rotate-keys')).toHaveLength(0);
});

test('a refused edit shows the refusal and sends nothing else', async () => {
  http.on('PUT', '/_/api/admin/users/1', json({ error: 'the access key id is already in use (UNIQUE constraint failed: users.access_key_id)' }, 409));
  await renameAndRekey('dana2', 'AKTAKEN0001');
  expect((await screen.findAllByText(/access key id is already in use/i)).length).toBeGreaterThan(0);
  expect(http.callsTo('PUT', '/_/api/admin/users/1')).toHaveLength(1);
  expect(http.callsTo('POST', '/_/api/admin/users/1/rotate-keys')).toHaveLength(0);
});
