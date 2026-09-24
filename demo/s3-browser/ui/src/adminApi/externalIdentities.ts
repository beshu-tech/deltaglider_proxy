// === External Identities ===
import { adminJson } from './core';

export interface ExternalIdentity {
  id: number;
  user_id: number;
  provider_id: number;
  external_sub: string;
  email?: string;
  display_name?: string;
  last_login?: string;
  raw_claims?: Record<string, unknown>;
  created_at: string;
}

export async function getExternalIdentities(): Promise<ExternalIdentity[]> {
  return adminJson('/api/admin/ext-auth/identities', { context: 'Load external identities' });
}

interface SyncResult {
  users_updated: number;
  memberships_changed: number;
}

export async function syncMemberships(): Promise<SyncResult> {
  return adminJson('/api/admin/ext-auth/sync-memberships', {
    method: 'POST',
    context: 'Config sync now',
  });
}
