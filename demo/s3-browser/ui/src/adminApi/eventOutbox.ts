// ─────────────────────────────────────────────────────────────
// Event outbox diagnostics
// ─────────────────────────────────────────────────────────────
import { throwApiError } from '../errorHandling';
import { adminFetch, fetchJson, safeJson } from './core';

export type EventOutboxStatus = 'pending' | 'in_progress' | 'delivered' | 'failed';

export interface EventOutboxRecord {
  id: number;
  kind: string;
  bucket: string;
  key: string;
  source: string;
  occurred_at: number;
  payload: unknown;
  status: EventOutboxStatus;
  attempts: number;
  next_attempt_at: number | null;
  claimed_by: string | null;
  claimed_at: number | null;
  delivered_at: number | null;
  last_error: string | null;
  created_at: number;
}

interface EventOutboxCounts {
  pending: number;
  in_progress: number;
  delivered: number;
  failed: number;
}

interface EventOutboxResponse {
  rows: EventOutboxRecord[];
  counts: EventOutboxCounts;
  total: number;
  limit: number;
  offset: number;
  status: EventOutboxStatus | null;
  sort: string;
  order: string;
  delivery_enabled: boolean;
  delivery_active: boolean;
}

interface EventOutboxRequeueResponse {
  requeued: number;
}

export async function fetchEventOutbox(
  limit = 100,
  status?: EventOutboxStatus | 'all',
  offset = 0,
  sort = 'occurred_at',
  order: 'asc' | 'desc' = 'desc',
): Promise<EventOutboxResponse> {
  const qs = new URLSearchParams({
    limit: String(limit),
    offset: String(offset),
    sort,
    order,
  });
  if (status && status !== 'all') qs.set('status', status);
  return fetchJson(`/api/admin/event-outbox?${qs.toString()}`, 'Event outbox fetch');
}

export async function requeueEventOutbox(id: number): Promise<EventOutboxRequeueResponse> {
  const res = await adminFetch(`/api/admin/event-outbox/${encodeURIComponent(id)}/requeue`, 'POST');
  if (!res.ok) await throwApiError(res, 'Event outbox requeue');
  return safeJson(res);
}

export async function requeueEventOutboxMany(ids: number[]): Promise<EventOutboxRequeueResponse> {
  const res = await adminFetch('/api/admin/event-outbox/requeue', 'POST', { ids });
  if (!res.ok) await throwApiError(res, 'Event outbox bulk requeue');
  return safeJson(res);
}

export async function purgeFailedEventOutbox(): Promise<{ purged: number }> {
  const res = await adminFetch('/api/admin/event-outbox/purge-failed', 'POST');
  if (!res.ok) await throwApiError(res, 'Event outbox purge failed');
  return safeJson(res);
}
