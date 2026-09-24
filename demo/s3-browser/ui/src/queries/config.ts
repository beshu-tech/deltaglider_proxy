/**
 * Admin config query.
 *
 * Only `useAdminConfig` / `useIamMode` are exported because they are the only
 * shapes call sites use. Add `useConfigYaml`, `useConfigSection`, etc. as
 * panels adopt them. Keeping unused exports here would just earn knip warnings.
 */
import { useQuery } from '@tanstack/react-query';
import { getAdminConfig, type AdminConfig, type IamMode } from '../adminApi';
import { normalizeUiError } from '../errorHandling';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { qk } from './keys';

export function useAdminConfig(options?: {
  enabled?: boolean;
  /** Called when the load fails because the session expired. */
  onSessionExpired?: () => void;
}) {
  const query = useQuery<AdminConfig>({
    queryKey: qk.config(),
    queryFn: getAdminConfig,
    // Callers without an admin session (e.g. InspectorPanel for an anonymous
    // public-bucket viewer) pass `enabled: false` to skip the fetch — it would
    // 403. Defaults to enabled so existing callers are unaffected.
    enabled: options?.enabled ?? true,
  });
  useSessionExpiredOn(query.error, options?.onSessionExpired);
  return query;
}

/**
 * Where IAM state lives, as the IAM panels must act on it. `readOnly` is true
 * in declarative mode AND while the config is not loaded (loading, or the load
 * failed): a declarative install 403s every IAM write, so the UI must not
 * enable mutation buttons on a guess. `loadError` explains a failed load.
 */
export function useIamMode(onSessionExpired?: () => void): {
  iamMode: IamMode | undefined;
  readOnly: boolean;
  loadError: string;
} {
  const { data, error } = useAdminConfig({ onSessionExpired });
  return {
    iamMode: data?.iam_mode,
    readOnly: !data || data.iam_mode === 'declarative',
    loadError: error ? normalizeUiError(error, 'Failed to load the IAM mode') : '',
  };
}
