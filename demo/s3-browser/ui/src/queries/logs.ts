/**
 * The proxy's log ring backlog, server-side filtered. The live tail (SSE)
 * stays in LogsPanel: a stream is not a query. No retry: a failure shows at
 * once, as the hand-rolled fetch did.
 */
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { fetchLogs, type LogFilters } from '../adminApi';
import { qk } from './keys';

export function useLogBacklog(filters: LogFilters, limit: number) {
  return useQuery({
    queryKey: qk.logs(filters.level ?? '', filters.target ?? '', filters.q ?? '', limit),
    queryFn: () => fetchLogs(filters, limit),
    placeholderData: keepPreviousData,
    retry: false,
  });
}
