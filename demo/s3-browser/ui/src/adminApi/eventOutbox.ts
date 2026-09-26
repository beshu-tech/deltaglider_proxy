// ─────────────────────────────────────────────────────────────
// Event outbox diagnostics
// ─────────────────────────────────────────────────────────────
import { adminJson } from './core';

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
  /** Per-endpoint delivery state (empty until an endpoint is attempted). */
  deliveries: EndpointDelivery[];
}

export interface EndpointDelivery {
  endpoint_id: string;
  /** Redacted URL or Slack channel; null when the endpoint left the config. */
  label: string | null;
  status: 'delivered' | 'failed';
  attempts: number;
  last_error: string | null;
  updated_at: number;
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
  /** `failing` = active, but the newest delivery failed. */
  delivery_state?: DeliveryState;
  last_delivery_error?: string | null;
}

export type DeliveryState = 'disabled' | 'no-endpoint' | 'active' | 'failing';

/** The delivery state of a response (an older server sends only the flags). */
export function deliveryStateOf(
  r: Pick<EventOutboxResponse, 'delivery_enabled' | 'delivery_active' | 'delivery_state'>,
): DeliveryState {
  if (r.delivery_state) return r.delivery_state;
  if (r.delivery_active) return 'active';
  return r.delivery_enabled ? 'no-endpoint' : 'disabled';
}

/** Tag label + colour per delivery state, shared by the delivery panels. */
export const DELIVERY_STATE: Record<DeliveryState, { label: string; color: string }> = {
  disabled: { label: 'Disabled', color: 'default' },
  'no-endpoint': { label: 'Enabled (no endpoint)', color: 'orange' },
  active: { label: 'Active', color: 'green' },
  failing: { label: 'Failing', color: 'red' },
};

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
  return adminJson(`/api/admin/event-outbox?${qs.toString()}`, { context: 'Event outbox fetch' });
}

export async function requeueEventOutbox(id: number): Promise<EventOutboxRequeueResponse> {
  return adminJson(`/api/admin/event-outbox/${encodeURIComponent(id)}/requeue`, {
    method: 'POST',
    context: 'Event outbox requeue',
  });
}

export async function requeueEventOutboxMany(ids: number[]): Promise<EventOutboxRequeueResponse> {
  return adminJson('/api/admin/event-outbox/requeue', {
    method: 'POST',
    body: { ids },
    context: 'Event outbox bulk requeue',
  });
}

export async function purgeFailedEventOutbox(): Promise<{ purged: number }> {
  return adminJson('/api/admin/event-outbox/purge-failed', {
    method: 'POST',
    context: 'Event outbox purge failed',
  });
}
