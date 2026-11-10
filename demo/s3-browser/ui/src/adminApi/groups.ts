// === IAM Group Management ===
import { adminJson, adminRequest } from './core';
import type { IamPermission } from './users';

export interface IamGroup {
  id: number;
  name: string;
  description: string;
  permissions: IamPermission[];
  member_ids: number[];
  created_at: string;
}

interface CreateGroupRequest {
  name: string;
  description?: string;
  permissions: IamPermission[];
}

/**
 * One edit of a group, in ONE request. The member change is the users to add
 * and to remove, never the whole list: other writers (another admin, a peer
 * node, an OIDC login) change members too, and a whole list would undo what
 * they did since the form opened. The server applies the change in the same
 * transaction as the rest, so a refusal of any part applies none.
 */
export interface UpdateGroupRequest {
  name?: string;
  description?: string;
  permissions?: IamPermission[];
  add_member_ids?: number[];
  remove_member_ids?: number[];
}

/**
 * The member change of a form: the members it added and removed against the
 * list it started from. Empty (no field at all) when nothing was toggled.
 */
export function memberDelta(
  start: readonly number[],
  now: ReadonlySet<number>,
): Pick<UpdateGroupRequest, 'add_member_ids' | 'remove_member_ids'> {
  const was = new Set(start);
  const add = [...now].filter((id) => !was.has(id)).sort((a, b) => a - b);
  const remove = start.filter((id) => !now.has(id)).sort((a, b) => a - b);
  return {
    ...(add.length > 0 ? { add_member_ids: add } : {}),
    ...(remove.length > 0 ? { remove_member_ids: remove } : {}),
  };
}

export async function getGroups(): Promise<IamGroup[]> {
  return adminJson('/api/admin/groups');
}

export async function createGroup(req: CreateGroupRequest): Promise<IamGroup> {
  return adminJson('/api/admin/groups', { method: 'POST', body: req });
}

export async function cloneGroup(
  id: number,
  req: { name?: string; copy_members?: boolean } = {},
): Promise<IamGroup> {
  return adminJson(`/api/admin/groups/${id}/clone`, { method: 'POST', body: req });
}

export async function updateGroup(id: number, req: UpdateGroupRequest): Promise<IamGroup> {
  return adminJson(`/api/admin/groups/${id}`, { method: 'PUT', body: req });
}

export async function deleteGroup(id: number): Promise<void> {
  await adminRequest(`/api/admin/groups/${id}`, {
    method: 'DELETE',
    context: `Delete group ${id}`,
  });
}

export async function addGroupMember(groupId: number, userId: number): Promise<void> {
  await adminRequest(`/api/admin/groups/${groupId}/members`, {
    method: 'POST',
    body: { user_id: userId },
    context: `Add member ${userId} to group ${groupId}`,
  });
}
