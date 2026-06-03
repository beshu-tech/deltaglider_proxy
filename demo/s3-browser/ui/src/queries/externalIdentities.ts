/**
 * External-identity (logged-in SSO users) read query.
 *
 * Read-only: identities are auto-provisioned on OAuth login; the UI only lists
 * them. A `syncMemberships()` POST re-derives group membership, after which the
 * caller invalidates `qk.externalIdentities.list()` + the groups/users lists.
 */
import { useQuery } from '@tanstack/react-query';
import { getExternalIdentities, type ExternalIdentity } from '../adminApi';
import { qk } from './keys';

export function useExternalIdentities() {
  return useQuery<ExternalIdentity[]>({
    queryKey: qk.externalIdentities.list(),
    queryFn: getExternalIdentities,
  });
}
