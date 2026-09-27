// === External Auth (OAuth/OIDC) ===
import { adminJson, adminRequest } from './core';

export interface AuthProvider {
  id: number;
  name: string;
  provider_type: string;
  enabled: boolean;
  priority: number;
  display_name?: string;
  client_id?: string;
  client_secret?: string;
  issuer_url?: string;
  scopes: string;
  extra_config?: Record<string, unknown>;
  created_at: string;
  updated_at: string;
}

interface CreateAuthProviderRequest {
  name: string;
  provider_type: string;
  enabled?: boolean;
  priority?: number;
  display_name?: string;
  client_id?: string;
  client_secret?: string;
  issuer_url?: string;
  scopes?: string;
  extra_config?: Record<string, unknown>;
}

interface UpdateAuthProviderRequest {
  name?: string;
  provider_type?: string;
  enabled?: boolean;
  priority?: number;
  display_name?: string;
  client_id?: string;
  client_secret?: string;
  issuer_url?: string;
  scopes?: string;
  extra_config?: Record<string, unknown>;
}

export interface ProviderTestResult {
  success: boolean;
  issuer?: string;
  authorization_endpoint?: string;
  error?: string;
}

export async function getAuthProviders(): Promise<AuthProvider[]> {
  return adminJson('/api/admin/ext-auth/providers', { context: 'Load auth providers' });
}

export async function createAuthProvider(req: CreateAuthProviderRequest): Promise<AuthProvider> {
  return adminJson('/api/admin/ext-auth/providers', {
    method: 'POST',
    body: req,
    context: 'Create auth provider',
  });
}

export async function updateAuthProvider(id: number, req: UpdateAuthProviderRequest): Promise<AuthProvider> {
  return adminJson(`/api/admin/ext-auth/providers/${id}`, {
    method: 'PUT',
    body: req,
    context: 'Update auth provider',
  });
}

export async function deleteAuthProvider(id: number): Promise<void> {
  await adminRequest(`/api/admin/ext-auth/providers/${id}`, {
    method: 'DELETE',
    context: 'Delete auth provider',
  });
}

/** The provider form as it is now, for a Test Connection that saves nothing.
 *  A blank `client_secret` keeps the saved secret. */
export interface ProviderTestForm {
  client_id: string;
  client_secret: string;
  issuer_url: string;
  scopes: string;
  extra_config: Record<string, unknown>;
}

/** Test a provider's discovery with the unsaved form: over the saved
 *  provider when `id` is set, else a provider that is not saved yet.
 *  A failed test is a 200 with `success: false`. */
export async function testAuthProvider(id: number | null, form: ProviderTestForm): Promise<ProviderTestResult> {
  const path = id === null
    ? '/api/admin/ext-auth/providers/test'
    : `/api/admin/ext-auth/providers/${id}/test`;
  return adminJson(path, {
    method: 'POST',
    body: form,
    context: 'Test auth provider',
  });
}
