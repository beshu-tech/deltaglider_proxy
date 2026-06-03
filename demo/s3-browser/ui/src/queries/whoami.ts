/**
 * Whoami read query.
 *
 * Identity + auth-mode + external-provider list for the current session.
 * `whoami()` already swallows errors (returns a bootstrap/null shape), so the
 * query never rejects. Keyed by `qk.whoami()`.
 */
import { useQuery } from '@tanstack/react-query';
import { whoami, type WhoamiResponse } from '../adminApi';
import { qk } from './keys';

export function useWhoami() {
  return useQuery<WhoamiResponse>({
    queryKey: qk.whoami(),
    queryFn: whoami,
  });
}
