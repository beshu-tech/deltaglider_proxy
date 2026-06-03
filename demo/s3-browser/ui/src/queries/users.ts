/**
 * IAM users queries + mutations.
 *
 * Pre-Query, every panel hand-rolled `useState<IamUser[]>`,
 * `useEffect(getUsers().then(...))`, and a `refresh()` callback that
 * had to be manually called from every mutation site. With Query, the
 * mutation closes the loop via `queryClient.invalidateQueries`.
 *
 * Only the hooks consumed by current call sites are exported — knip
 * blocks dead exports.
 */
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  getUsers,
  createUser,
  updateUser,
  deleteUser,
  cloneUser,
  rotateUserKeys,
  type IamUser,
  type CreateUserRequest,
  type UpdateUserRequest,
} from '../adminApi';
import { qk } from './keys';

export function useUsers() {
  return useQuery({
    queryKey: qk.users.list(),
    queryFn: getUsers,
  });
}

function useInvalidateUsers() {
  const qc = useQueryClient();
  return () => {
    qc.invalidateQueries({ queryKey: qk.users.list() });
  };
}

export function useCreateUser() {
  const invalidate = useInvalidateUsers();
  return useMutation<IamUser, Error, CreateUserRequest>({
    mutationFn: createUser,
    onSuccess: invalidate,
  });
}

interface UpdateUserVars {
  id: number;
  patch: UpdateUserRequest;
}

export function useUpdateUser() {
  const invalidate = useInvalidateUsers();
  return useMutation<IamUser, Error, UpdateUserVars>({
    mutationFn: ({ id, patch }) => updateUser(id, patch),
    onSuccess: invalidate,
  });
}

export function useDeleteUser() {
  const invalidate = useInvalidateUsers();
  return useMutation<void, Error, number>({
    mutationFn: deleteUser,
    onSuccess: invalidate,
  });
}

interface CloneUserVars {
  id: number;
  name?: string;
  copyGroupMemberships?: boolean;
}

export function useCloneUser() {
  const invalidate = useInvalidateUsers();
  return useMutation<IamUser, Error, CloneUserVars>({
    mutationFn: ({ id, name, copyGroupMemberships }) =>
      cloneUser(id, { name, copy_group_memberships: copyGroupMemberships }),
    onSuccess: invalidate,
  });
}

interface RotateUserKeysVars {
  id: number;
  accessKeyId?: string;
  secretAccessKey?: string;
}

export function useRotateUserKeys() {
  const invalidate = useInvalidateUsers();
  return useMutation<IamUser, Error, RotateUserKeysVars>({
    mutationFn: ({ id, accessKeyId, secretAccessKey }) =>
      rotateUserKeys(id, accessKeyId, secretAccessKey),
    onSuccess: invalidate,
  });
}
