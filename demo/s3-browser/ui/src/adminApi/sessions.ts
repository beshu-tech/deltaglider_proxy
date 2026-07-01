// === Admin session management (list + force-logout) ===
import { throwApiError } from '../errorHandling';
import { adminFetch, fetchJson, safeJson } from './core';

export interface SessionSummary {
  id: string;
  ip: string | null;
  age_secs: number;
  admin_gui: boolean;
  auth: string;
  identity: string | null;
}

export async function listSessions(): Promise<SessionSummary[]> {
  const res = await fetchJson<{ sessions: SessionSummary[] }>('/api/admin/sessions', 'List sessions');
  return res.sessions;
}

export async function revokeSession(id: string): Promise<void> {
  const res = await adminFetch(`/api/admin/sessions/${encodeURIComponent(id)}`, 'DELETE');
  if (!res.ok) await throwApiError(res, 'Revoke session');
}

export interface RevokeUserResult {
  revoked: number;
  revoked_local: number;
  persisted: boolean;
  pushed: boolean;
  propagation_bound_secs: number | null;
}

export async function revokeUserSessions(identity: string): Promise<RevokeUserResult> {
  const res = await adminFetch('/api/admin/sessions/revoke-user', 'POST', { identity });
  if (!res.ok) await throwApiError(res, 'Revoke user sessions');
  return safeJson(res);
}
