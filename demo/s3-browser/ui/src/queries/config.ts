/**
 * Admin config query.
 *
 * Only `useAdminConfig` is exported because it's the only call site
 * that's been migrated. Add `useConfigYaml`, `useConfigSection`,
 * `useApplyConfigYaml`, `usePutSection`, etc. as panels adopt them.
 * Keeping unused exports here would just earn knip warnings.
 */
import { useQuery } from '@tanstack/react-query';
import { getAdminConfig, type AdminConfig } from '../adminApi';
import { qk } from './keys';

export function useAdminConfig() {
  return useQuery<AdminConfig | null>({
    queryKey: qk.config(),
    queryFn: getAdminConfig,
  });
}
