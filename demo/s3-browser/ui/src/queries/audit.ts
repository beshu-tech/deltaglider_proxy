/**
 * The in-memory audit ring (read-only). The panel's auto-refresh switch sets
 * the poll interval; no retry, so a failure shows at once.
 */
import { useQuery } from '@tanstack/react-query';
import { fetchAudit } from '../adminApi';
import { qk } from './keys';

/** The ring is bounded (500 by default), so the panel reads all of it. */
const AUDIT_LIMIT = 500;

export function useAuditLog(refetchInterval: number | false) {
  return useQuery({
    queryKey: qk.audit(AUDIT_LIMIT),
    queryFn: () => fetchAudit(AUDIT_LIMIT),
    refetchInterval,
    retry: false,
  });
}
