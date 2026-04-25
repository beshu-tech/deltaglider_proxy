/**
 * IAM users queries + mutations.
 *
 * `useUsers()` reads the current list. `useCreateUser()`,
 * `useUpdateUser()`, `useDeleteUser()`, `useRotateUserKeys()` are
 * mutations that automatically invalidate the list on success.
 *
 * Pre-Query, every panel hand-rolled `useState<IamUser[]>`,
 * `useEffect(getUsers().then(...))`, and a `refresh()` callback that
 * had to be manually called from every mutation site. With Query, the
 * mutation closes the loop via `queryClient.invalidateQueries`.
 */
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  getUsers,
  createUser,
  updateUser,
  deleteUser,
  rotateUserKeys,
  getCannedPolicies,
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

export function useCannedPolicies() {
  return useQuery({
    queryKey: qk.users.cannedPolicies(),
    queryFn: getCannedPolicies,
    // Static-ish list; deduplicate aggressively across panels.
    staleTime: 5 * 60_000,
  });
}

export function useCreateUser() {
  const qc = useQueryClient();
  return useMutation<IamUser, Error, CreateUserRequest>({
    mutationFn: createUser,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.users.list() });
    },
  });
}

export function useUpdateUser() {
  const qc = useQueryClient();
  return useMutation<IamUser, Error, { id: number; req: UpdateUserRequest }>({
    mutationFn: ({ id, req }) => updateUser(id, req),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.users.list() });
    },
  });
}

export function useDeleteUser() {
  const qc = useQueryClient();
  return useMutation<void, Error, number>({
    mutationFn: deleteUser,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.users.list() });
    },
  });
}

export function useRotateUserKeys() {
  const qc = useQueryClient();
  return useMutation<
    IamUser,
    Error,
    { id: number; accessKeyId?: string; secretAccessKey?: string }
  >({
    mutationFn: ({ id, accessKeyId, secretAccessKey }) =>
      rotateUserKeys(id, accessKeyId, secretAccessKey),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.users.list() });
    },
  });
}
