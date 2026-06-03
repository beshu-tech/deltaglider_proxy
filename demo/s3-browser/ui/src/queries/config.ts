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

export function useAdminConfig(options?: { enabled?: boolean }) {
  return useQuery<AdminConfig | null>({
    queryKey: qk.config(),
    queryFn: getAdminConfig,
    // Callers without an admin session (e.g. InspectorPanel for an anonymous
    // public-bucket viewer) pass `enabled: false` to skip the fetch — it would
    // 403. Defaults to enabled so existing callers are unaffected.
    enabled: options?.enabled ?? true,
  });
}
