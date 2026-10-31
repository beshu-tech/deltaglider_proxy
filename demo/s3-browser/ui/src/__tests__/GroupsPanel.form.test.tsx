/**
 * B21: an empty group name was a banner at the top of the form; the field
 * itself was not marked. B23: an empty page said "nothing here" twice.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import GroupsPanel from '../components/GroupsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/groups', json([]));
  http.on('GET', '/_/api/admin/users', json([]));
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui', env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('an empty name is marked on the field and nothing is sent', async () => {
  const user = userEvent.setup();
  renderWithQuery(<GroupsPanel />);
  await user.click(await screen.findByTestId('md-new-groups'));
  await user.click(screen.getByRole('button', { name: 'Create group' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Enter a name.');
  expect(screen.getByRole('textbox', { name: 'Name' })).toHaveAttribute('aria-invalid', 'true');
  expect(http.callsTo('POST', '/_/api/admin/groups')).toHaveLength(0);
});

test('with no groups the page shows one empty state, not two', async () => {
  renderWithQuery(<GroupsPanel />);
  expect(await screen.findByText('Create the first group')).toBeInTheDocument();
  expect(screen.queryByText(/No groups/)).toBeNull();
});

// C1: group names follow THE IAM name rule, like user names.
test('a name with spaces is refused on the field with the user-name rule', async () => {
  const user = userEvent.setup();
  renderWithQuery(<GroupsPanel />);
  await user.click(await screen.findByTestId('md-new-groups'));
  await user.type(screen.getByRole('textbox', { name: 'Name' }), 'qa group with spaces!');
  await user.click(screen.getByRole('button', { name: 'Create group' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Remove the spaces. Use letters, digits and + = , . @ _ - only.');
  expect(http.callsTo('POST', '/_/api/admin/groups')).toHaveLength(0);
});

test('the server name refusal shows on the field', async () => {
  http.on('POST', '/_/api/admin/groups', json({ error: 'invalid_request', message: 'group name cannot start with \'$\'' }, 400));
  const user = userEvent.setup();
  renderWithQuery(<GroupsPanel />);
  await user.click(await screen.findByTestId('md-new-groups'));
  await user.type(screen.getByRole('textbox', { name: 'Name' }), 'developers');
  await user.click(screen.getByRole('button', { name: 'Create group' }));
  expect(await screen.findByRole('alert')).toHaveTextContent("Group name cannot start with '$'.");
  expect(screen.getByRole('textbox', { name: 'Name' })).toHaveAttribute('aria-invalid', 'true');
});

test('a bad group name shows its error on blur, like a user name (UX harvest D46)', async () => {
  const user = userEvent.setup();
  renderWithQuery(<GroupsPanel />);
  await user.click(await screen.findByTestId('md-new-groups'));
  await user.type(screen.getByTestId('group-name'), 'qa group with spaces!');
  await user.tab();
  expect(screen.getByTestId('group-name')).toHaveAttribute('aria-invalid', 'true');
  expect(screen.getByText(/Remove the spaces/)).toBeInTheDocument();
});
