/**
 * F23 (epic #146), groups: saving a group edit is ONE request. The rename and
 * the member changes used to go as a `PUT` and then one request per member; a
 * failure in the middle left a half-applied edit. Now `member_ids` rides on
 * the one `PUT`.
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

test('a rename with member changes is one PUT with the whole member list', async () => {
  const user = userEvent.setup();
  renderWithQuery(<GroupsPanel initialGroupId={7} />);
  const name = await screen.findByTestId('group-name');
  await user.clear(name);
  await user.type(name, 'dev2');
  await user.click(screen.getByRole('checkbox', { name: 'bob' }));
  await user.click(screen.getByRole('checkbox', { name: 'dana' }));
  await user.click(screen.getByTestId('group-save'));
  await waitFor(() => expect(http.callsTo('PUT', '/_/api/admin/groups/7')).toHaveLength(1));
  expect(http.callsTo('PUT', '/_/api/admin/groups/7')[0].body).toMatchObject({ name: 'dev2', member_ids: [2] });
  expect(http.calls.filter((c) => c.path.includes('/members'))).toHaveLength(0);
});
