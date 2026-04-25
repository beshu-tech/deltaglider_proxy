/**
 * IAM users queries + mutations.
 *
 * Pre-Query, every panel hand-rolled `useState<IamUser[]>`,
 * `useEffect(getUsers().then(...))`, and a `refresh()` callback that
 * had to be manually called from every mutation site. With Query, the
 * mutation closes the loop via `queryClient.invalidateQueries`.
 *
 * Only the hooks consumed by current call sites are exported. Add more
 * (createUser/updateUser/rotateUserKeys/...) when a panel actually
 * adopts them — knip blocks dead exports.
 */
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { getUsers, deleteUser } from '../adminApi';
import { qk } from './keys';

export function useUsers() {
  return useQuery({
    queryKey: qk.users.list(),
    queryFn: getUsers,
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
