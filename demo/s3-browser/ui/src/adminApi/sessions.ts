// === Admin session management (list + force-logout) ===
import { adminJson, adminRequest } from './core';

export interface SessionSummary {
  id: string;
  ip: string | null;
  age_secs: number;
  admin_gui: boolean;
  auth: string;
  identity: string | null;
  /** The session making this request (it cannot revoke itself). */
  current: boolean;
}

export async function listSessions(): Promise<SessionSummary[]> {
  const res = await adminJson<{ sessions: SessionSummary[] }>('/api/admin/sessions', {
    context: 'List sessions',
  });
  return res.sessions;
}

export async function revokeSession(id: string): Promise<void> {
  await adminRequest(`/api/admin/sessions/${encodeURIComponent(id)}`, {
    method: 'DELETE',
    context: 'Revoke session',
  });
}

export interface RevokeUserResult {
  revoked: number;
  revoked_local: number;
  persisted: boolean;
  pushed: boolean;
  propagation_bound_secs: number | null;
}

export async function revokeUserSessions(identity: string): Promise<RevokeUserResult> {
  return adminJson('/api/admin/sessions/revoke-user', {
    method: 'POST',
    body: { identity },
    context: 'Revoke user sessions',
  });
}
