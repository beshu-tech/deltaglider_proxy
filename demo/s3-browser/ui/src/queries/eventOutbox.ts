/**
 * Event outbox reads + requeue/purge.
 *
 * Every write invalidates the whole `qk.eventOutbox.all()` family, so the
 * page, its counts and the delivery settings strip all refresh. No retry on
 * the reads: a failure shows at once, as the hand-rolled fetches did.
 */
import { keepPreviousData, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  fetchEventOutbox,
  purgeFailedEventOutbox,
  requeueEventOutbox,
  requeueEventOutboxMany,
  type EventOutboxStatus,
} from '../adminApi';
import { qk } from './keys';

export interface EventOutboxPageParams {
  limit: number;
  offset: number;
  status: EventOutboxStatus | 'all';
  sort: string;
  order: 'asc' | 'desc';
}

/** One server page. The previous page stays on screen while the next loads. */
export function useEventOutboxPage(p: EventOutboxPageParams, refetchInterval: number | false) {
  return useQuery({
    queryKey: qk.eventOutbox.page(`${p.limit}:${p.offset}:${p.status}:${p.sort}:${p.order}`),
    queryFn: () => fetchEventOutbox(p.limit, p.status, p.offset, p.sort, p.order),
    placeholderData: keepPreviousData,
    refetchInterval,
    retry: false,
  });
}

/** Counts + delivery state only (best-effort status strip). */
export function useEventOutboxSummary() {
  return useQuery({
    queryKey: qk.eventOutbox.summary(),
    queryFn: () => fetchEventOutbox(1),
    retry: false,
  });
}

function useInvalidateOutbox() {
  const qc = useQueryClient();
  return () => qc.invalidateQueries({ queryKey: qk.eventOutbox.all() });
}

export function useRequeueEvent() {
  const invalidate = useInvalidateOutbox();
  return useMutation({ mutationFn: (id: number) => requeueEventOutbox(id), onSuccess: invalidate });
}

export function useRequeueEvents() {
  const invalidate = useInvalidateOutbox();
  return useMutation({ mutationFn: (ids: number[]) => requeueEventOutboxMany(ids), onSuccess: invalidate });
}

export function usePurgeFailedEvents() {
  const invalidate = useInvalidateOutbox();
  return useMutation({ mutationFn: () => purgeFailedEventOutbox(), onSuccess: invalidate });
}
