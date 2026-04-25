/**
 * Config queries (flat + sectioned + YAML).
 *
 * All three views of the same underlying server config — invalidating
 * one invalidates all. `apply` and `putSection` mutations cascade.
 */
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  getAdminConfig,
  exportConfigYaml,
  applyConfigYaml,
  validateConfigYaml,
  getSection,
  putSection,
  validateSection,
  type AdminConfig,
  type ConfigApplyResponse,
  type ConfigValidateResponse,
  type SectionName,
  type SectionApplyResponse,
} from '../adminApi';
import { qk } from './keys';

export function useAdminConfig() {
  return useQuery<AdminConfig | null>({
    queryKey: qk.config(),
    queryFn: getAdminConfig,
  });
}

export function useConfigYaml() {
  return useQuery<string>({
    queryKey: qk.configYaml(),
    queryFn: exportConfigYaml,
  });
}

export function useConfigSection<T = unknown>(section: SectionName) {
  return useQuery<T>({
    queryKey: qk.configSection(section),
    queryFn: () => getSection<T>(section),
  });
}

/** Invalidate every config-derived view. Used by mutation `onSuccess`. */
function invalidateAllConfig(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: qk.config() });
  qc.invalidateQueries({ queryKey: qk.configYaml() });
  qc.invalidateQueries({ queryKey: qk.backends.list() });
}

export function useApplyConfigYaml() {
  const qc = useQueryClient();
  return useMutation<ConfigApplyResponse, Error, string>({
    mutationFn: applyConfigYaml,
    onSuccess: () => invalidateAllConfig(qc),
  });
}

export function useValidateConfigYaml() {
  return useMutation<ConfigValidateResponse, Error, string>({
    mutationFn: validateConfigYaml,
  });
}

export function usePutSection<T = unknown>() {
  const qc = useQueryClient();
  return useMutation<SectionApplyResponse, Error, { section: SectionName; body: T }>({
    mutationFn: ({ section, body }) => putSection<T>(section, body),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: qk.configSection(vars.section) });
      invalidateAllConfig(qc);
    },
  });
}

export function useValidateSection<T = unknown>() {
  return useMutation<SectionApplyResponse, Error, { section: SectionName; body: T }>({
    mutationFn: ({ section, body }) => validateSection<T>(section, body),
  });
}
