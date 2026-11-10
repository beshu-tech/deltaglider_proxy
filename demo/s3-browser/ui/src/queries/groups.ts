/**
 * IAM groups queries + mutations.
 *
 * Mirrors `queries/users.ts`: a `useGroups` read keyed by `qk.groups.list()`,
 * and per-record mutations that close the cache loop via
 * `queryClient.invalidateQueries`. Group membership changes (add/remove) and
 * any create/update/delete all invalidate the groups list; because group
 * membership affects a user's effective permissions, they also invalidate the
 * users list. Group state lives in the encrypted IAM DB, so these do not touch
 * `qk.config()`.
 *
 * Only hooks that a panel actually consumes are exported — knip blocks dead
 * exports.
 */
import { useMutation, useQuery } from '@tanstack/react-query';
import {
  getGroups,
  createGroup,
  updateGroup,
  type UpdateGroupRequest,
  deleteGroup,
  cloneGroup,
  addGroupMember,
  type IamGroup,
} from '../adminApi';
import type { IamPermission } from '../adminApi';
import { qk } from './keys';
import { useInvalidateIamLists } from './users';

export function useGroups() {
  return useQuery({
    queryKey: qk.groups.list(),
    queryFn: getGroups,
  });
}

interface CreateGroupVars {
  name: string;
  description?: string;
  permissions: IamPermission[];
}

export function useCreateGroup() {
  const invalidate = useInvalidateIamLists();
  return useMutation<IamGroup, Error, CreateGroupVars>({
    mutationFn: (vars) => createGroup(vars),
    onSuccess: invalidate,
  });
}

interface UpdateGroupVars {
  id: number;
  patch: UpdateGroupRequest;
}

export function useUpdateGroup() {
  const invalidate = useInvalidateIamLists();
  return useMutation<IamGroup, Error, UpdateGroupVars>({
    mutationFn: ({ id, patch }) => updateGroup(id, patch),
    onSuccess: invalidate,
  });
}

export function useDeleteGroup() {
  const invalidate = useInvalidateIamLists();
  return useMutation<void, Error, number>({
    mutationFn: deleteGroup,
    onSuccess: invalidate,
  });
}

interface CloneGroupVars {
  id: number;
  copyMembers?: boolean;
}

export function useCloneGroup() {
  const invalidate = useInvalidateIamLists();
  return useMutation<IamGroup, Error, CloneGroupVars>({
    mutationFn: ({ id, copyMembers }) => cloneGroup(id, { copy_members: copyMembers }),
    onSuccess: invalidate,
  });
}

interface GroupMemberVars {
  groupId: number;
  userId: number;
}

export function useAddGroupMember() {
  const invalidate = useInvalidateIamLists();
  return useMutation<void, Error, GroupMemberVars>({
    mutationFn: ({ groupId, userId }) => addGroupMember(groupId, userId),
    onSuccess: invalidate,
  });
}
