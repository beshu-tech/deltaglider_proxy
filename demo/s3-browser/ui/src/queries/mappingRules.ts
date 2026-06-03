/**
 * Group-mapping-rule queries + mutations.
 *
 * Mirrors `queries/users.ts`: a `useGroupMappingRules` read keyed by
 * `qk.groupMappingRules.list()`, and per-record mutations that invalidate that
 * key on success. Mapping rules live in the encrypted IAM DB, so these do NOT
 * invalidate `qk.config()`.
 *
 * Only hooks consumed by a panel are exported — knip blocks dead exports.
 */
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  getMappingRules,
  createMappingRule,
  updateMappingRule,
  deleteMappingRule,
  type MappingRule,
} from '../adminApi';
import { qk } from './keys';

export function useGroupMappingRules() {
  return useQuery({
    queryKey: qk.groupMappingRules.list(),
    queryFn: getMappingRules,
  });
}

function useInvalidateMappingRules() {
  const qc = useQueryClient();
  return () => {
    qc.invalidateQueries({ queryKey: qk.groupMappingRules.list() });
  };
}

type CreateMappingRuleVars = Parameters<typeof createMappingRule>[0];

export function useCreateMappingRule() {
  const invalidate = useInvalidateMappingRules();
  return useMutation<MappingRule, Error, CreateMappingRuleVars>({
    mutationFn: (vars) => createMappingRule(vars),
    onSuccess: invalidate,
  });
}

interface UpdateMappingRuleVars {
  id: number;
  patch: Parameters<typeof updateMappingRule>[1];
}

export function useUpdateMappingRule() {
  const invalidate = useInvalidateMappingRules();
  return useMutation<MappingRule, Error, UpdateMappingRuleVars>({
    mutationFn: ({ id, patch }) => updateMappingRule(id, patch),
    onSuccess: invalidate,
  });
}

export function useDeleteMappingRule() {
  const invalidate = useInvalidateMappingRules();
  return useMutation<void, Error, number>({
    mutationFn: deleteMappingRule,
    onSuccess: invalidate,
  });
}
