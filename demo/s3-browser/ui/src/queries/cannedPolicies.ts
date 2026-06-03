/**
 * Canned-policy presets read query.
 *
 * Read-only and static for the lifetime of a session — the underlying
 * `getCannedPolicies()` already swallows errors and returns `[]`, so the query
 * never rejects. Used to seed the permission-preset pills in UserForm.
 */
import { useQuery } from '@tanstack/react-query';
import { getCannedPolicies, type CannedPolicy } from '../adminApi';
import { qk } from './keys';

export function useCannedPolicies() {
  return useQuery<CannedPolicy[]>({
    queryKey: qk.users.cannedPolicies(),
    queryFn: getCannedPolicies,
  });
}
