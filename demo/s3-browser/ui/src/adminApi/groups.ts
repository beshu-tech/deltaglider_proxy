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

interface UpdateGroupRequest {
  name?: string;
  description?: string;
  permissions?: IamPermission[];
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

export async function removeGroupMember(groupId: number, userId: number): Promise<void> {
  await adminRequest(`/api/admin/groups/${groupId}/members/${userId}`, {
    method: 'DELETE',
    context: `Remove member ${userId} from group ${groupId}`,
  });
}
