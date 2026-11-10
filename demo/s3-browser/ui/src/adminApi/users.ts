// === IAM User Management ===
import { adminJson, adminRequest } from './core';

export interface IamPermission {
  id: number;
  effect?: string; // "Allow" or "Deny", defaults to "Allow"
  actions: string[];
  resources: string[];
  conditions?: Record<string, Record<string, string | string[]>>;
}

export interface IamUser {
  id: number;
  name: string;
  access_key_id: string;
  secret_access_key?: string;
  enabled: boolean;
  created_at: string;
  permissions: IamPermission[];
  /** Group IDs this user belongs to. Populated by the server on every
   *  `/users` fetch. Used by the list panel to distinguish a user with
   *  no direct policies but inherited permissions from a truly-no-access
   *  user (UX-5). */
  group_ids?: number[];
  auth_source?: string; // "local" or "external"
}

export interface CreateUserRequest {
  name: string;
  access_key_id?: string;
  secret_access_key?: string;
  enabled?: boolean;
  permissions: IamPermission[];
}

/**
 * One edit of a user, in ONE request. `access_key_id` / `secret_access_key`
 * rotate the keys in the same transaction (a refusal of any part applies
 * none); the answer then carries the new secret, shown once. A secret that
 * is not given is generated.
 */
export interface UpdateUserRequest {
  name?: string;
  enabled?: boolean;
  permissions?: IamPermission[];
  access_key_id?: string;
  secret_access_key?: string;
}

export async function getUsers(): Promise<IamUser[]> {
  return adminJson('/api/admin/users');
}

export async function createUser(req: CreateUserRequest): Promise<IamUser> {
  return adminJson('/api/admin/users', { method: 'POST', body: req });
}

export async function cloneUser(
  id: number,
  req: { name?: string; copy_group_memberships?: boolean } = {},
): Promise<IamUser> {
  return adminJson(`/api/admin/users/${id}/clone`, { method: 'POST', body: req });
}

export async function updateUser(id: number, req: UpdateUserRequest): Promise<IamUser> {
  return adminJson(`/api/admin/users/${id}`, { method: 'PUT', body: req });
}

export async function deleteUser(id: number): Promise<void> {
  await adminRequest(`/api/admin/users/${id}`, { method: 'DELETE', context: `Delete user ${id}` });
}
