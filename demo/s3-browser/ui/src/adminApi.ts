// Admin API client helpers

const BASE = '/_';

/** Shared fetch wrapper — handles credentials, JSON serialization, content-type. */
export async function adminFetch(path: string, method = 'GET', body?: unknown): Promise<Response> {
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
  // Session is destroyed server-side — S3 credentials are cleared with it
}

export interface AdminConfig {
  listen_addr: string;
  backend_type: string;
  backend_path: string | null;
  backend_endpoint: string | null;
  backend_region: string | null;
  backend_force_path_style: boolean | null;
  backend_has_credentials: boolean;
  // Compression
  max_delta_ratio: number;
  max_object_size: number;
  cache_size_mb: number;
  metadata_cache_mb: number;
  codec_concurrency: number;
  codec_timeout_secs: number;
  // Limits
  request_timeout_secs: number;
  max_concurrent_requests: number;
  max_multipart_uploads: number;
  // Auth
  auth_enabled: boolean;
  access_key_id: string | null;
  // Security
  clock_skew_seconds: number;
  replay_window_secs: number;
  rate_limit_max_attempts: number;
  rate_limit_window_secs: number;
  rate_limit_lockout_secs: number;
  session_ttl_hours: number;
  trust_proxy_headers: boolean;
  secure_cookies: boolean;
  debug_headers: boolean;
  // Sync
  config_sync_bucket: string | null;
  // Per-bucket policies
  bucket_policies: Record<string, { compression?: boolean; max_delta_ratio?: number; backend?: string; alias?: string; public_prefixes?: string[] }>;
  // Multi-backend
  backends: BackendInfo[];
  default_backend: string | null;
  // Logging
  log_level: string;
  // Taint detection
  tainted_fields: string[];
}

export interface BackendInfo {
  name: string;
  backend_type: string;
  path: string | null;
  endpoint: string | null;
  region: string | null;
  force_path_style: boolean | null;
  has_credentials: boolean;
}

export async function getAdminConfig(): Promise<AdminConfig | null> {
  const res = await adminFetch('/api/admin/config');
  if (!res.ok) return null;
  return safeJson(res);
}

export async function checkSession(): Promise<boolean> {
  try {
    const res = await adminFetch('/api/admin/session');
    if (!res.ok) return false;
    const data = await safeJson<{ valid?: boolean }>(res);
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

/** Safely parse JSON from response, falling back to text for non-JSON content types. */
async function safeJson<T>(res: Response): Promise<T> {
  const ct = res.headers.get('content-type') || '';
  if (ct.includes('application/json')) {
    return res.json();
  }
  const text = await res.text();
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(text || `Unexpected response (${res.status})`);
  }
}

export async function updateAdminConfig(updates: Record<string, unknown>): Promise<ConfigUpdateResponse> {
  const res = await adminFetch('/api/admin/config', 'PUT', updates);
  if (!res.ok) throw new Error(`Config update failed: ${res.status}`);
  return safeJson(res);
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
  return safeJson(res);
}

export async function changeAdminPassword(
  currentPassword: string,
  newPassword: string
): Promise<PasswordChangeResponse> {
  const res = await adminFetch('/api/admin/password', 'PUT', {
    current_password: currentPassword,
    new_password: newPassword,
  });
  return safeJson(res);
}

// === IAM User Management ===

export interface IamPermission {
  id: number;
  effect?: string; // "Allow" or "Deny", defaults to "Allow"
  actions: string[];
  resources: string[];
  conditions?: Record<string, Record<string, string | string[]>>;
}

// === Canned Policies ===

export interface CannedPolicy {
  name: string;
  description: string;
  permissions: IamPermission[];
}

export async function getCannedPolicies(): Promise<CannedPolicy[]> {
  try {
    const res = await adminFetch('/api/admin/policies');
    if (!res.ok) return [];
    return safeJson(res);
  } catch {
    return [];
  }
}

export interface IamUser {
  id: number;
  name: string;
  access_key_id: string;
  secret_access_key?: string;
  enabled: boolean;
  created_at: string;
  permissions: IamPermission[];
  auth_source?: string; // "local" or "external"
}

export interface CreateUserRequest {
  name: string;
  access_key_id?: string;
  secret_access_key?: string;
  enabled?: boolean;
  permissions: IamPermission[];
}

export interface UpdateUserRequest {
  name?: string;
  enabled?: boolean;
  permissions?: IamPermission[];
}

export async function getUsers(): Promise<IamUser[]> {
  const res = await adminFetch('/api/admin/users');
  if (!res.ok) throw new Error(`Failed to load users: ${res.status}`);
  return safeJson(res);
}

export async function createUser(req: CreateUserRequest): Promise<IamUser> {
  const res = await adminFetch('/api/admin/users', 'POST', req);
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(text || `Failed to create user: ${res.status}`);
  }
  return safeJson(res);
}

export async function updateUser(id: number, req: UpdateUserRequest): Promise<IamUser> {
  const res = await adminFetch(`/api/admin/users/${id}`, 'PUT', req);
  if (!res.ok) throw new Error(`Failed to update user: ${res.status}`);
  return safeJson(res);
}

export async function deleteUser(id: number): Promise<void> {
  const res = await adminFetch(`/api/admin/users/${id}`, 'DELETE');
  if (!res.ok) throw new Error(`Failed to delete user: ${res.status}`);
}

export async function rotateUserKeys(
  id: number,
  accessKeyId?: string,
  secretAccessKey?: string,
): Promise<IamUser> {
  const body: Record<string, string> = {};
  if (accessKeyId) body.access_key_id = accessKeyId;
  if (secretAccessKey) body.secret_access_key = secretAccessKey;
  const res = await adminFetch(
    `/api/admin/users/${id}/rotate-keys`,
    'POST',
    Object.keys(body).length > 0 ? body : undefined,
  );
  if (!res.ok) throw new Error(`Failed to rotate keys: ${res.status}`);
  return safeJson(res);
}

// === IAM Group Management ===

export interface IamGroup {
  id: number;
  name: string;
  description: string;
  permissions: IamPermission[];
  member_ids: number[];
  created_at: string;
}

export interface CreateGroupRequest {
  name: string;
  description?: string;
  permissions: IamPermission[];
}

export interface UpdateGroupRequest {
  name?: string;
  description?: string;
  permissions?: IamPermission[];
}

export async function getGroups(): Promise<IamGroup[]> {
  const res = await adminFetch('/api/admin/groups');
  if (!res.ok) throw new Error(`Failed to load groups: ${res.status}`);
  return safeJson(res);
}

export async function createGroup(req: CreateGroupRequest): Promise<IamGroup> {
  const res = await adminFetch('/api/admin/groups', 'POST', req);
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(text || `Failed to create group: ${res.status}`);
  }
  return safeJson(res);
}

export async function updateGroup(id: number, req: UpdateGroupRequest): Promise<IamGroup> {
  const res = await adminFetch(`/api/admin/groups/${id}`, 'PUT', req);
  if (!res.ok) throw new Error(`Failed to update group: ${res.status}`);
  return safeJson(res);
}

export async function deleteGroup(id: number): Promise<void> {
  const res = await adminFetch(`/api/admin/groups/${id}`, 'DELETE');
  if (!res.ok) throw new Error(`Failed to delete group: ${res.status}`);
}

export async function addGroupMember(groupId: number, userId: number): Promise<void> {
  const res = await adminFetch(`/api/admin/groups/${groupId}/members`, 'POST', { user_id: userId });
  if (!res.ok) throw new Error(`Failed to add member: ${res.status}`);
}

export async function removeGroupMember(groupId: number, userId: number): Promise<void> {
  const res = await adminFetch(`/api/admin/groups/${groupId}/members/${userId}`, 'DELETE');
  if (!res.ok) throw new Error(`Failed to remove member: ${res.status}`);
}

// === Whoami / Login-as ===

export interface ExternalProviderInfo {
  name: string;
  type: string;
  display_name: string;
}

export interface WhoamiResponse {
  mode: 'bootstrap' | 'iam' | 'open';
  version?: string;
  user: { name: string; access_key_id: string; is_admin: boolean } | null;
  config_db_mismatch?: boolean;
  external_providers?: ExternalProviderInfo[];
}

export async function whoami(): Promise<WhoamiResponse> {
  try {
    const res = await adminFetch('/api/whoami');
    if (!res.ok) return { mode: 'bootstrap', user: null };
    return await safeJson(res);
  } catch (err) {
    console.warn('whoami request failed:', err);
    return { mode: 'bootstrap', user: null };
  }
}

// === Usage Scanner ===

export interface ChildUsage {
  size: number;
  objects: number;
}

export interface UsageEntry {
  prefix: string;
  bucket: string;
  total_size: number;
  total_objects: number;
  children: Record<string, ChildUsage>;
  computed_at: string;
  stale_seconds: number;
}

/** Trigger a background usage scan for a bucket/prefix. */
export async function scanPrefixUsage(bucket: string, prefix: string): Promise<void> {
  const res = await adminFetch('/api/admin/usage/scan', 'POST', { bucket, prefix });
  if (!res.ok) throw new Error(`Scan request failed: ${res.status}`);
}

/** Get cached usage entry for a bucket/prefix, or null if not cached. */
export async function getPrefixUsage(bucket: string, prefix: string): Promise<UsageEntry | null> {
  const params = new URLSearchParams({ bucket, prefix });
  const res = await adminFetch(`/api/admin/usage?${params}`);
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`Usage query failed: ${res.status}`);
  return safeJson(res);
}

// === IAM Backup/Restore ===

export async function exportBackup(): Promise<unknown> {
  const res = await adminFetch('/api/admin/backup');
  if (!res.ok) throw new Error(`Export failed: ${res.status}`);
  return safeJson(res);
}

export async function importBackup(data: unknown): Promise<{ users_created: number; groups_created: number; users_skipped: number; groups_skipped: number }> {
  const res = await adminFetch('/api/admin/backup', 'POST', data);
  if (!res.ok) throw new Error(`Import failed: ${res.status}`);
  return safeJson(res);
}

export async function loginAs(accessKeyId: string, secretAccessKey: string): Promise<{ ok: boolean; error?: string }> {
  const res = await adminFetch('/api/admin/login-as', 'POST', {
    access_key_id: accessKeyId,
    secret_access_key: secretAccessKey,
  });
  if (res.ok) return { ok: true };
  return { ok: false, error: 'Admin access denied — invalid credentials or insufficient permissions' };
}

// === Multi-Backend Management ===

export interface BackendListResponse {
  backends: BackendInfo[];
  default_backend: string | null;
}

export interface CreateBackendRequest {
  name: string;
  type: string;
  path?: string;
  endpoint?: string;
  region?: string;
  force_path_style?: boolean;
  access_key_id?: string;
  secret_access_key?: string;
  set_default?: boolean;
}

export async function getBackends(): Promise<BackendListResponse> {
  const res = await adminFetch('/api/admin/backends');
  if (!res.ok) throw new Error(`Failed to load backends: ${res.status}`);
  return safeJson(res);
}

export async function createBackend(req: CreateBackendRequest): Promise<{ success: boolean; error?: string }> {
  const res = await adminFetch('/api/admin/backends', 'POST', req);
  return safeJson(res);
}

export async function deleteBackend(name: string): Promise<{ success: boolean; error?: string }> {
  const res = await adminFetch(`/api/admin/backends/${encodeURIComponent(name)}`, 'DELETE');
  return safeJson(res);
}

// === Config DB Recovery ===

export interface RecoverDbResponse {
  success: boolean;
  correct_hash?: string;
  correct_hash_base64?: string;
  error?: string;
}

export async function recoverDb(candidatePassword: string): Promise<RecoverDbResponse> {
  const res = await adminFetch('/api/admin/recover-db', 'POST', {
    candidate_password: candidatePassword,
  });
  return safeJson(res);
}

// === External Auth (OAuth/OIDC) ===

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

export interface CreateAuthProviderRequest {
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

export interface UpdateAuthProviderRequest {
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
  const res = await adminFetch('/api/admin/ext-auth/providers');
  if (!res.ok) throw new Error(`Failed to load providers: ${res.status}`);
  return safeJson(res);
}

export async function createAuthProvider(req: CreateAuthProviderRequest): Promise<AuthProvider> {
  const res = await adminFetch('/api/admin/ext-auth/providers', 'POST', req);
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(text || `Failed to create provider: ${res.status}`);
  }
  return safeJson(res);
}

export async function updateAuthProvider(id: number, req: UpdateAuthProviderRequest): Promise<AuthProvider> {
  const res = await adminFetch(`/api/admin/ext-auth/providers/${id}`, 'PUT', req);
  if (!res.ok) throw new Error(`Failed to update provider: ${res.status}`);
  return safeJson(res);
}

export async function deleteAuthProvider(id: number): Promise<void> {
  const res = await adminFetch(`/api/admin/ext-auth/providers/${id}`, 'DELETE');
  if (!res.ok) throw new Error(`Failed to delete provider: ${res.status}`);
}

export async function testAuthProvider(id: number): Promise<ProviderTestResult> {
  const res = await adminFetch(`/api/admin/ext-auth/providers/${id}/test`, 'POST');
  if (!res.ok) throw new Error(`Test failed: ${res.status}`);
  return safeJson(res);
}

// === Group Mapping Rules ===

export interface MappingRule {
  id: number;
  provider_id: number | null;
  priority: number;
  match_type: string;
  match_field: string;
  match_value: string;
  group_id: number;
  created_at: string;
}

export interface CreateMappingRuleRequest {
  provider_id?: number | null;
  priority?: number;
  match_type: string;
  match_field?: string;
  match_value: string;
  group_id: number;
}

export interface UpdateMappingRuleRequest {
  provider_id?: number | null;
  priority?: number;
  match_type?: string;
  match_field?: string;
  match_value?: string;
  group_id?: number;
}

export async function getMappingRules(): Promise<MappingRule[]> {
  const res = await adminFetch('/api/admin/ext-auth/mappings');
  if (!res.ok) throw new Error(`Failed to load mappings: ${res.status}`);
  return safeJson(res);
}

export async function createMappingRule(req: CreateMappingRuleRequest): Promise<MappingRule> {
  const res = await adminFetch('/api/admin/ext-auth/mappings', 'POST', req);
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(text || `Failed to create mapping: ${res.status}`);
  }
  return safeJson(res);
}

export async function updateMappingRule(id: number, req: UpdateMappingRuleRequest): Promise<MappingRule> {
  const res = await adminFetch(`/api/admin/ext-auth/mappings/${id}`, 'PUT', req);
  if (!res.ok) throw new Error(`Failed to update mapping: ${res.status}`);
  return safeJson(res);
}

export async function deleteMappingRule(id: number): Promise<void> {
  const res = await adminFetch(`/api/admin/ext-auth/mappings/${id}`, 'DELETE');
  if (!res.ok) throw new Error(`Failed to delete mapping: ${res.status}`);
}

export interface MappingPreviewResponse {
  group_ids: number[];
  group_names: string[];
}

export async function previewMapping(email: string): Promise<MappingPreviewResponse> {
  const res = await adminFetch('/api/admin/ext-auth/mappings/preview', 'POST', { email });
  if (!res.ok) throw new Error(`Preview failed: ${res.status}`);
  return safeJson(res);
}

// === External Identities ===

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
  const res = await adminFetch('/api/admin/ext-auth/identities');
  if (!res.ok) throw new Error(`Failed to load identities: ${res.status}`);
  return safeJson(res);
}

export interface SyncResult {
  users_updated: number;
  memberships_changed: number;
}

export async function syncMemberships(): Promise<SyncResult> {
  const res = await adminFetch('/api/admin/ext-auth/sync-memberships', 'POST');
  if (!res.ok) throw new Error(`Sync failed: ${res.status}`);
  return safeJson(res);
}
