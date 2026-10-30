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
  await user.click(await screen.findByRole('button', { name: /new/i }));
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
