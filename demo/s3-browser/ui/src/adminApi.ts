// Admin API client helpers

const BASE = '';

/** Shared fetch wrapper — handles credentials, JSON serialization, content-type. */
async function adminFetch(path: string, method = 'GET', body?: unknown): Promise<Response> {
  const opts: RequestInit = { method, credentials: 'include' };
  if (body !== undefined) {
    opts.headers = { 'Content-Type': 'application/json' };
    opts.body = JSON.stringify(body);
  }
  return fetch(`${BASE}${path}`, opts);
}

export async function adminLogin(password: string): Promise<{ ok: boolean; error?: string }> {
  const res = await adminFetch('/api/admin/login', 'POST', { password });
  if (res.ok) return { ok: true };
  try {
    const data = await res.json();
    return { ok: false, error: data.error || 'Login failed' };
  } catch {
    return { ok: false, error: 'Login failed' };
  }
}

export async function adminLogout(): Promise<void> {
  await adminFetch('/api/admin/logout', 'POST');
}

export interface AdminConfig {
  listen_addr: string;
  backend_type: string;
  backend_path: string | null;
  backend_endpoint: string | null;
  backend_region: string | null;
  max_delta_ratio: number;
  max_object_size: number;
  cache_size_mb: number;
  auth_enabled: boolean;
  access_key_id: string | null;
  log_level: string;
  backend_has_credentials: boolean;
  backend_force_path_style: boolean | null;
}

export async function getAdminConfig(): Promise<AdminConfig | null> {
  const res = await adminFetch('/api/admin/config');
  if (!res.ok) return null;
  return res.json();
}

export async function checkSession(): Promise<boolean> {
  try {
    const res = await adminFetch('/api/admin/session');
    if (!res.ok) return false;
    const data = await res.json();
    return data.valid === true;
  } catch {
    return false;
  }
}

export interface ConfigUpdateResponse {
  success: boolean;
  warnings: string[];
  requires_restart: boolean;
}

export async function updateAdminConfig(updates: Record<string, unknown>): Promise<ConfigUpdateResponse> {
  const res = await adminFetch('/api/admin/config', 'PUT', updates);
  return res.json();
}

export interface PasswordChangeResponse {
  ok: boolean;
  error?: string;
}

export interface TestS3Request {
  endpoint?: string;
  region?: string;
  force_path_style?: boolean;
  access_key_id?: string;
  secret_access_key?: string;
}

export interface TestS3Response {
  success: boolean;
  buckets?: string[];
  error?: string;
  error_kind?: string;
}

export async function testS3Connection(req: TestS3Request): Promise<TestS3Response> {
  const res = await adminFetch('/api/admin/test-s3', 'POST', req);
  return res.json();
}

export async function changeAdminPassword(
  currentPassword: string,
  newPassword: string
): Promise<PasswordChangeResponse> {
  const res = await adminFetch('/api/admin/password', 'PUT', {
    current_password: currentPassword,
    new_password: newPassword,
  });
  return res.json();
}
