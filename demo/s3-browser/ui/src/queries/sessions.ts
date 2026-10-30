/**
 * Live admin sessions (in-memory on the proxy) + force-logout.
 *
 * Both revokes invalidate the list. No retry: a failed read shows at once,
 * as the hand-rolled fetch did.
 */
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { listSessions, revokeSession, revokeUserSessions, type RevokeUserResult } from '../adminApi';
import { qk } from './keys';

export function useSessions() {
  return useQuery({ queryKey: qk.sessions.list(), queryFn: listSessions, retry: false });
}

function useInvalidateSessions() {
  const qc = useQueryClient();
  return () => qc.invalidateQueries({ queryKey: qk.sessions.list() });
}

export function useRevokeSession() {
  const invalidate = useInvalidateSessions();
  return useMutation<void, Error, string>({ mutationFn: revokeSession, onSuccess: invalidate });
}

export function useRevokeUserSessions() {
  const invalidate = useInvalidateSessions();
  return useMutation<RevokeUserResult, Error, string>({ mutationFn: revokeUserSessions, onSuccess: invalidate });
}
