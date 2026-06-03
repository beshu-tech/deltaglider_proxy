/**
 * External-auth (OAuth/OIDC) provider queries + mutations.
 *
 * Mirrors `queries/users.ts`: a `useAuthProviders` read keyed by
 * `qk.authProviders.list()`, and per-record mutations that invalidate that key
 * on success. Providers live in the encrypted IAM DB (not YAML in `gui` mode),
 * so these mutations do NOT invalidate `qk.config()`.
 *
 * Only hooks consumed by a panel are exported — knip blocks dead exports.
 */
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  getAuthProviders,
  createAuthProvider,
  updateAuthProvider,
  deleteAuthProvider,
  type AuthProvider,
} from '../adminApi';
import { qk } from './keys';

export function useAuthProviders() {
  return useQuery({
    queryKey: qk.authProviders.list(),
    queryFn: getAuthProviders,
  });
}

function useInvalidateAuthProviders() {
  const qc = useQueryClient();
  return () => {
    qc.invalidateQueries({ queryKey: qk.authProviders.list() });
  };
}

type CreateAuthProviderVars = Parameters<typeof createAuthProvider>[0];

export function useCreateAuthProvider() {
  const invalidate = useInvalidateAuthProviders();
  return useMutation<AuthProvider, Error, CreateAuthProviderVars>({
    mutationFn: (vars) => createAuthProvider(vars),
    onSuccess: invalidate,
  });
}

interface UpdateAuthProviderVars {
  id: number;
  patch: Parameters<typeof updateAuthProvider>[1];
}

export function useUpdateAuthProvider() {
  const invalidate = useInvalidateAuthProviders();
  return useMutation<AuthProvider, Error, UpdateAuthProviderVars>({
    mutationFn: ({ id, patch }) => updateAuthProvider(id, patch),
    onSuccess: invalidate,
  });
}

export function useDeleteAuthProvider() {
  const invalidate = useInvalidateAuthProviders();
  return useMutation<void, Error, number>({
    mutationFn: deleteAuthProvider,
    onSuccess: invalidate,
  });
}
