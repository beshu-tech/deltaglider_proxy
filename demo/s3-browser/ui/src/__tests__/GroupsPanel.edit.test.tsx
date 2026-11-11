/**
 * F23 (epic #146), groups: saving a group edit is ONE request. The rename and
 * the member changes used to go as a `PUT` and then one request per member; a
 * failure in the middle left a half-applied edit. Now the member changes ride
 * on the one `PUT`, as add/remove deltas against the list the form started
 * from: a whole list would undo what other writers changed since the form
 * opened.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import GroupsPanel from '../components/GroupsPanel';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const GROUP = { id: 7, name: 'dev', description: '', permissions: [], member_ids: [1] };
const USERS = [
  { id: 1, name: 'dana', access_key_id: 'AKDANA', enabled: true, created_at: '', permissions: [], group_ids: [7] },
  { id: 2, name: 'bob', access_key_id: 'AKBOB', enabled: true, created_at: '', permissions: [], group_ids: [] },
];
let http: ReturnType<typeof mockFetch>;

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/groups', json([GROUP]));
  http.on('GET', '/_/api/admin/users', json(USERS));
  http.on('GET', '/_/api/admin/config', json({ iam_mode: 'gui', env_overrides: [] }));
  http.on('GET', '/_/api/admin/policies', json([]));
  http.on('PUT', '/_/api/admin/groups/7', json({ ...GROUP, name: 'dev2', member_ids: [2] }));
  for (const method of ['POST', 'DELETE']) {
    http.on(method, /\/_\/api\/admin\/groups\/7\/members/, () => {
      throw new Error('a group edit must not send one request per member');
    });
  }
});
afterEach(() => vi.unstubAllGlobals());

test('a rename with member changes is one PUT with the members added and removed', async () => {
  const user = userEvent.setup();
  renderWithQuery(<GroupsPanel initialGroupId={7} />);
  const name = await screen.findByTestId('group-name');
  await user.clear(name);
  await user.type(name, 'dev2');
  await user.click(screen.getByRole('checkbox', { name: 'bob' }));
  await user.click(screen.getByRole('checkbox', { name: 'dana' }));
  await user.click(screen.getByTestId('group-save'));
  await waitFor(() => expect(http.callsTo('PUT', '/_/api/admin/groups/7')).toHaveLength(1));
  expect(http.callsTo('PUT', '/_/api/admin/groups/7')[0].body).toMatchObject({
    name: 'dev2',
    add_member_ids: [2],
    remove_member_ids: [1],
  });
  expect(http.callsTo('PUT', '/_/api/admin/groups/7')[0].body).not.toHaveProperty('member_ids');
  expect(http.calls.filter((c) => c.path.includes('/members'))).toHaveLength(0);
});

test('a save that did not touch the members sends no member field', async () => {
  // Another admin (or an OIDC login) may have changed the members since the
  // form opened; a description-only save must leave that alone.
  const user = userEvent.setup();
  renderWithQuery(<GroupsPanel initialGroupId={7} />);
  const description = await screen.findByPlaceholderText('e.g. Development team access');
  await user.type(description, 'the dev team');
  await user.click(screen.getByTestId('group-save'));
  await waitFor(() => expect(http.callsTo('PUT', '/_/api/admin/groups/7')).toHaveLength(1));
  const body = http.callsTo('PUT', '/_/api/admin/groups/7')[0].body as Record<string, unknown>;
  expect(body).toMatchObject({ description: 'the dev team' });
  for (const field of ['member_ids', 'add_member_ids', 'remove_member_ids']) {
    expect(body).not.toHaveProperty(field);
  }
});
