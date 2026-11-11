/**
 * F63 class guard (epic #146): which query keys each IAM mutation
 * invalidates. A group lists its member ids, a user lists its group ids, so
 * a write on one side must refresh the other. Deleting a user removes it
 * from its groups; cloning a user can copy its memberships. Both left the
 * group list stale.
 */
import { act } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { qk } from '../queries/keys';
import { useCreateUser, useDeleteUser, useCloneUser, useUpdateUser } from '../queries/users';
import { useAddGroupMember, useCloneGroup, useCreateGroup, useDeleteGroup, useUpdateGroup } from '../queries/groups';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery } from '../test/render';

const USERS = JSON.stringify(qk.users.list());
const GROUPS = JSON.stringify(qk.groups.list());

beforeEach(() => {
  const http = mockFetch();
  const ok = json({ id: 1, name: 'x', access_key_id: 'AK', permissions: [], member_ids: [] });
  for (const [method, path] of [
    ['POST', '/_/api/admin/users'],
    ['PUT', '/_/api/admin/users/1'],
    ['DELETE', '/_/api/admin/users/1'],
    ['POST', '/_/api/admin/users/1/clone'],
    ['POST', '/_/api/admin/groups'],
    ['PUT', '/_/api/admin/groups/1'],
    ['DELETE', '/_/api/admin/groups/1'],
    ['POST', '/_/api/admin/groups/1/clone'],
    ['POST', '/_/api/admin/groups/1/members'],
  ] as const) {
    http.on(method, path, () => (method === 'DELETE' ? new Response(null, { status: 204 }) : ok.clone()));
  }
});
afterEach(() => vi.unstubAllGlobals());

/** The query keys a mutation invalidates when it succeeds. */
async function invalidatedBy(useHook: () => { mutateAsync: (vars: never) => Promise<unknown> }, vars: unknown): Promise<string[]> {
  const { result, client } = renderHookWithQuery(useHook);
  const spy = vi.spyOn(client, 'invalidateQueries');
  await act(async () => {
    await result.current.mutateAsync(vars as never);
  });
  return spy.mock.calls.map(([filter]) => JSON.stringify(filter?.queryKey)).sort();
}

const BOTH = [GROUPS, USERS].sort();

describe('the query keys each IAM mutation invalidates', () => {
  test.each([
    ['create user', () => useCreateUser(), { name: 'x', permissions: [] }, [USERS]],
    ['update user', () => useUpdateUser(), { id: 1, patch: { name: 'x' } }, [USERS]],
    // The groups list shows member ids: the user leaves its groups.
    ['delete user', () => useDeleteUser(), 1, BOTH],
    // A clone can copy the group memberships.
    ['clone user', () => useCloneUser(), { id: 1, name: 'y', copyGroupMemberships: true }, BOTH],
    ['create group', () => useCreateGroup(), { name: 'x', permissions: [] }, BOTH],
    ['update group', () => useUpdateGroup(), { id: 1, patch: { name: 'x' } }, BOTH],
    ['delete group', () => useDeleteGroup(), 1, BOTH],
    ['clone group', () => useCloneGroup(), { id: 1, copyMembers: true }, BOTH],
    ['add group member', () => useAddGroupMember(), { groupId: 1, userId: 2 }, BOTH],
  ] as const)('%s', async (_name, useHook, vars, expected) => {
    expect(await invalidatedBy(useHook as never, vars)).toEqual([...expected].sort());
  });
});
