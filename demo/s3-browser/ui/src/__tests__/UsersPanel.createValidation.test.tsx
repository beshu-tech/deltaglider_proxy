/**
 * Create-user validation and the first-user switch.
 *
 * B3: the form accepted any name ("bad name with spaces!!" was created). It
 * now checks the server's name rule on the field and sends nothing.
 * B4: the first IAM user switches sign-in to IAM mode and keeps the bootstrap
 * key as `legacy-admin`; Create asks first and the credentials dialog says so.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import UsersPanel from '../components/UsersPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
let users: Array<Record<string, unknown>>;
const dana = { id: 1, name: 'dana', access_key_id: 'AKDANA', enabled: true, created_at: '', permissions: [], group_ids: [] };

beforeEach(() => {
  users = [dana];
  http = mockFetch();
  http.on('GET', '/_/api/admin/users', () => json(users));
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui', access_key_id: 'AKBOOTSTRAP', env_overrides: [] }));
  http.on('GET', '/_/api/admin/policies', json([]));
  http.on('GET', '/_/api/admin/groups', json([]));
  http.on('POST', '/_/api/admin/users', () => {
    const created = { id: 2, name: 'ci-uploader', access_key_id: 'AKNEW', enabled: true, created_at: '', permissions: [], group_ids: [] };
    const legacy = { id: 3, name: 'legacy-admin', access_key_id: 'AKBOOTSTRAP', enabled: true, created_at: '', permissions: [], group_ids: [] };
    users = users.length === 0 ? [created, legacy] : [...users, created];
    return json({ ...created, secret_access_key: 'S3CRET' });
  });
});
afterEach(() => vi.unstubAllGlobals());

const lastDialog = async () => {
  const dialogs = await screen.findAllByRole('dialog');
  return dialogs[dialogs.length - 1];
};

test('a name with spaces is refused on the field and nothing is sent', async () => {
  const user = userEvent.setup();
  renderWithQuery(<UsersPanel />);
  await user.click(await screen.findByRole('button', { name: /new/i }));
  const name = screen.getByRole('textbox', { name: 'Name' });
  await user.type(name, 'bad name with spaces!!');
  await user.click(screen.getByRole('button', { name: 'Create user' }));
  expect(await screen.findByRole('alert')).toHaveTextContent(/Remove the spaces/);
  expect(name).toHaveAttribute('aria-invalid', 'true');
  expect(http.callsTo('POST', '/_/api/admin/users')).toHaveLength(0);
  // Fixing the name clears the error and the create goes through.
  await user.clear(name);
  await user.type(name, 'ci-uploader');
  expect(screen.queryByRole('alert')).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Create user' }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/users')).toHaveLength(1));
});

test('an empty name is marked on the field, not in a banner', async () => {
  const user = userEvent.setup();
  renderWithQuery(<UsersPanel />);
  await user.click(await screen.findByRole('button', { name: /new/i }));
  await user.click(screen.getByRole('button', { name: 'Create user' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Enter a name.');
  expect(screen.getByRole('textbox', { name: 'Name' })).toHaveAttribute('aria-invalid', 'true');
});

test('the server refusal of a name shows on the field', async () => {
  http.on('POST', '/_/api/admin/users', json({ error: 'invalid_request', message: 'user name cannot start with \'$\'' }, 400));
  const user = userEvent.setup();
  renderWithQuery(<UsersPanel />);
  await user.click(await screen.findByRole('button', { name: /new/i }));
  // Only the server knows this one: a name the GUI rule passes.
  await user.type(screen.getByRole('textbox', { name: 'Name' }), 'ok-name');
  await user.click(screen.getByRole('button', { name: 'Create user' }));
  await waitFor(() => expect(http.callsTo('POST', '/_/api/admin/users')).toHaveLength(1));
  expect(await screen.findByRole('alert')).toBeInTheDocument();
  expect(screen.getByRole('textbox', { name: 'Name' })).toHaveAttribute('aria-invalid', 'true');
});

test('the first user asks before it switches sign-in, and the credentials dialog says so', async () => {
  users = [];
  const user = userEvent.setup();
  renderWithQuery(<UsersPanel />);
  await user.click(await screen.findByRole('button', { name: /new/i }));
  await user.type(screen.getByRole('textbox', { name: 'Name' }), 'ci-uploader');
  await user.click(screen.getByRole('button', { name: 'Create user' }));
  const confirm = await lastDialog();
  expect(within(confirm).getAllByText('Switch sign-in to IAM users?').length).toBeGreaterThan(0);
  expect(confirm).toHaveTextContent(/AKBOOTSTRAP becomes the user legacy-admin/);
  expect(http.callsTo('POST', '/_/api/admin/users')).toHaveLength(0);
  await user.click(within(confirm).getByRole('button', { name: 'Create user and switch' }));
  const creds = await screen.findByRole('dialog', { name: /save these credentials/i });
  // C64: the confirm explained the switch; the credentials dialog says it in one line.
  expect(creds).toHaveTextContent('You now sign in with an access key.');
  expect(creds).not.toHaveTextContent(/legacy-admin/);
});

test('a later user does not ask about the switch', async () => {
  const user = userEvent.setup();
  renderWithQuery(<UsersPanel />);
  await user.click(await screen.findByRole('button', { name: /new/i }));
  await user.type(screen.getByRole('textbox', { name: 'Name' }), 'ci-uploader');
  await user.click(screen.getByRole('button', { name: 'Create user' }));
  const creds = await screen.findByRole('dialog', { name: /save these credentials/i });
  expect(creds).not.toHaveTextContent(/legacy-admin|You now sign in/);
});

// C62: a preset on an untouched new-user form replaces the default rule at once.
test('a preset on a new form, or after another preset, applies without a confirm', async () => {
  const user = userEvent.setup();
  renderWithQuery(<UsersPanel />);
  await user.click(await screen.findByRole('button', { name: /new/i }));
  await user.click(screen.getByRole('button', { name: 'Read/Write/Delete' }));
  await user.click(screen.getByRole('button', { name: 'Read Only' }));
  expect(screen.queryByRole('dialog')).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Full Access (admin)' }));
  expect(screen.queryByRole('dialog')).toBeNull();
});

test('with no users the page shows one empty state, not two', async () => {
  users = [];
  renderWithQuery(<UsersPanel />);
  expect(await screen.findByText('Create the first user')).toBeInTheDocument();
  expect(screen.queryByText(/No users/)).toBeNull();
});
