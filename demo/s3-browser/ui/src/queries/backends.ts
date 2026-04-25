/**
 * Backends queries + mutations.
 *
 * Backends are global state — when one is added/removed, every panel
 * that displays backend info should refetch. Mutations invalidate
 * `qk.backends.list` AND `qk.config` (the backend list mirrors into
 * the flat config response that BackendsPanel summary cards read).
 */
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  getBackends,
  createBackend,
  deleteBackend,
  type BackendListResponse,
  type CreateBackendRequest,
} from '../adminApi';
import { qk } from './keys';

export function useBackends() {
  return useQuery<BackendListResponse>({
    queryKey: qk.backends.list(),
    queryFn: getBackends,
  });
}

export function useCreateBackend() {
  const qc = useQueryClient();
  return useMutation<{ success: boolean; error?: string }, Error, CreateBackendRequest>({
    mutationFn: createBackend,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.backends.list() });
      // Backends are reflected in `config` too.
      qc.invalidateQueries({ queryKey: qk.config() });
    },
  });
}

export function useDeleteBackend() {
  const qc = useQueryClient();
  return useMutation<{ success: boolean; error?: string }, Error, string>({
    mutationFn: deleteBackend,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.backends.list() });
      qc.invalidateQueries({ queryKey: qk.config() });
    },
  });
}
