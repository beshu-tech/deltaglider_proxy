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
  bucket_policies: Record<
    string,
    {
      compression?: boolean;
      max_delta_ratio?: number;
      backend?: string;
      alias?: string;
      /**
       * Key prefixes with anonymous read access. The canonical
       * "entire bucket is public" representation is a single empty
       * string `[""]` — the PublicPrefixSnapshot treats the empty
       * prefix as matching every key. Round-trips to/from `public: true`
       * via the backend's `BucketPolicyConfig::normalize` /
       * `collapse_to_shorthand`.
       */
      public_prefixes?: string[];
      /**
       * Shorthand form (Phase 3b.1): `public: true` expands to
       * `public_prefixes: [""]` server-side. The admin API accepts
       * either form on PATCH; responses always carry the expanded
       * form so the UI doesn't need to handle both at display time.
       */
      public?: boolean;
      quota_bytes?: number;
    }
  >;
  // Multi-backend
  backends: BackendInfo[];
  default_backend: string | null;
  // Logging
  log_level: string;
  // Operator-authored admission blocks (Phase 3b.2). The new
  // Admission tab in the admin UI reads/writes this. Round-tripped
  // verbatim — no client-side transformation.
  admission_blocks: AdmissionBlock[];
  // IAM source-of-truth mode (Phase 3c.1). `"gui"` (DB authoritative)
  // or `"declarative"` (YAML authoritative; IAM mutation routes 403).
  iam_mode: IamMode;
  // Taint detection
  tainted_fields: string[];
  // Encryption-at-rest status (no key material — just presence).
  // Drives the EncryptionPanel status badge and the per-bucket
  // "Encrypted at rest" indicator in BucketsPanel.
  encryption_enabled: boolean;
}

export type IamMode = 'gui' | 'declarative';

/**
 * Operator-authored admission block. Structure mirrors the backend
 * `AdmissionBlockSpec` — round-tripped verbatim through PATCH /config.
 *
 * Validation (duplicate names, bad Reject status, source_ip_list cap,
 * path_glob syntax, reserved `public-prefix:*` name prefix) runs
 * server-side at PATCH time; clients should display any resulting
 * `warnings` strings to the operator.
 */
export interface AdmissionBlock {
  name: string;
  match: AdmissionMatch;
  action: AdmissionAction;
}

export interface AdmissionMatch {
  method?: string[];
  source_ip?: string;
  source_ip_list?: string[];
  bucket?: string;
  path_glob?: string;
  authenticated?: boolean;
  config_flag?: string;
}

export type AdmissionAction =
  | 'allow-anonymous'
  | 'deny'
  | 'continue'
  | { type: 'reject'; status: number; message?: string };

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

interface ConfigUpdateResponse {
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

/**
 * Fetch the current runtime config as canonical YAML (four-section
 * shape, secrets redacted). Backs the "Copy as YAML" / "Export"
 * button flows. Returns the raw YAML string — the UI renders it
 * syntax-highlighted in a modal.
 */
export async function exportConfigYaml(): Promise<string> {
  const res = await adminFetch('/api/admin/config/export');
  if (!res.ok) throw new Error(`Config export failed: ${res.status}`);
  return res.text();
}

interface ConfigValidateResponse {
  ok: boolean;
  warnings: string[];
  error?: string;
}

/**
 * Dry-run a YAML document against the live server's validator. No
 * runtime state is mutated. The "Import YAML" flow uses this before
 * showing a confirm-apply dialog.
 */
export async function validateConfigYaml(yaml: string): Promise<ConfigValidateResponse> {
  const res = await adminFetch('/api/admin/config/validate', 'POST', { yaml });
  return safeJson(res);
}

export interface ConfigApplyResponse {
  applied: boolean;
  persisted: boolean;
  requires_restart: boolean;
  warnings: string[];
  error?: string;
  persisted_path?: string;
}

/**
 * Apply a full YAML config document. The server runs validation,
 * merges runtime secrets forward, atomically swaps the in-memory
 * config, and persists to disk. Admin GUI's "Import from YAML" and
 * "Paste YAML" flows both terminate here.
 */
export async function applyConfigYaml(yaml: string): Promise<ConfigApplyResponse> {
  const res = await adminFetch('/api/admin/config/apply', 'POST', { yaml });
  return safeJson(res);
}

// ═══════════════════════════════════════════════════════════════════
// Section-level config API (Wave 1 of the admin UI revamp).
// ═══════════════════════════════════════════════════════════════════

/** Four top-level sections of the YAML config, matching the sidebar groups. */
export type SectionName = 'admission' | 'access' | 'storage' | 'advanced';

/**
 * Response from section PUT / validate. Mirrors `SectionApplyResponse`
 * on the backend. The `diff` field drives the plan-diff-apply dialog
 * (§5.3 of the admin UI revamp plan).
 */
export interface SectionApplyResponse {
  ok: boolean;
  warnings?: string[];
  requires_restart?: boolean;
  persisted_path?: string;
  error?: string;
  /**
   * `{ section: { "field.path": { before, after } } }`. Only present
   * when validation succeeded far enough to compute a diff — malformed
   * bodies return `ok: false` without a diff.
   */
  diff?: Record<string, Record<string, { before: unknown; after: unknown }>>;
}

/**
 * Fetch one top-level section of the runtime config. Returns the
 * parsed JSON body — the shape is the section's type
 * (`AdmissionSection` / `AccessSection` / `StorageSection` /
 * `AdvancedSection` on the backend). Secrets are redacted.
 *
 * Empty-default sections return `{}`; the UI treats absent keys as
 * defaults and lets `FormField`'s placeholder carry the default
 * value.
 */
export async function getSection<T = unknown>(section: SectionName): Promise<T> {
  const res = await adminFetch(`/api/admin/config/section/${section}`);
  if (!res.ok) throw new Error(`Section fetch failed (${section}): ${res.status}`);
  return safeJson(res);
}

/**
 * Fetch one section as canonical YAML text (the `?format=yaml`
 * variant). Backs the per-section Copy-as-YAML button.
 */
export async function getSectionYaml(section: SectionName): Promise<string> {
  const res = await adminFetch(`/api/admin/config/section/${section}?format=yaml`);
  if (!res.ok) throw new Error(`Section YAML fetch failed (${section}): ${res.status}`);
  return res.text();
}

/**
 * Apply a section body. On success: the section slice is swapped
 * in-memory, side effects (engine rebuild, log reload, IAM state,
 * snapshot rebuilds) fire, and the on-disk config file is
 * rewritten. Response carries a diff for the Apply dialog.
 */
export async function putSection<T = unknown>(
  section: SectionName,
  body: T
): Promise<SectionApplyResponse> {
  const res = await adminFetch(`/api/admin/config/section/${section}`, 'PUT', body);
  return safeJson(res);
}

/**
 * Dry-run a section body. No runtime mutation, no persist. Returns
 * `{ ok, warnings, requires_restart, diff }` for the plan step of
 * the plan-diff-apply dialog.
 */
export async function validateSection<T = unknown>(
  section: SectionName,
  body: T
): Promise<SectionApplyResponse> {
  const res = await adminFetch(
    `/api/admin/config/section/${section}/validate`,
    'POST',
    body
  );
  return safeJson(res);
}

interface PasswordChangeResponse {
  ok: boolean;
  error?: string;
}

interface TestS3Request {
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
  /** Group IDs this user belongs to. Populated by the server on every
   *  `/users` fetch. Used by the list panel to distinguish a user with
   *  no direct policies but inherited permissions from a truly-no-access
   *  user (UX-5). */
  group_ids?: number[];
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

interface CreateGroupRequest {
  name: string;
  description?: string;
  permissions: IamPermission[];
}

interface UpdateGroupRequest {
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

interface ChildUsage {
  size: number;
  objects: number;
}

interface UsageEntry {
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

// === Full Backup / Restore ===
//
// Since v0.8.4 the default shape is a zip containing config.yaml +
// iam.json + secrets.json + manifest.json. The legacy IAM-only JSON
// export stays addressable via `?format=json` for backwards compat,
// but every admin GUI flow uses the zip exclusively.

/**
 * Download the Full Backup as a zip Blob. Callers pipe this into a
 * File-Saver-style `<a download>` dance; the caller owns the saved
 * filename (typically derived from the Content-Disposition header).
 */
export async function exportBackup(): Promise<{ blob: Blob; filename: string }> {
  const res = await adminFetch('/api/admin/backup');
  if (!res.ok) throw new Error(`Export failed: ${res.status}`);
  // Parse the server-suggested filename from Content-Disposition
  // (server emits `attachment; filename="dgp-backup-vX.Y.Z-<utc>.zip"`).
  const cd = res.headers.get('content-disposition') ?? '';
  const m = cd.match(/filename="?([^";]+)"?/i);
  const filename =
    m?.[1] ?? `dgp-backup-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, '')}.zip`;
  const blob = await res.blob();
  return { blob, filename };
}

interface ImportBackupResult {
  users_created: number;
  users_skipped: number;
  groups_created: number;
  groups_skipped: number;
  memberships_created: number;
  external_identities_created?: number;
  external_identities_skipped?: number;
}

/**
 * Restore from a backup file. Accepts either:
 *   - a `File` / `Blob` of a zip exported by this server (posts as
 *     `application/zip`, goes through the full-backup import path
 *     which applies config.yaml + iam.json + secrets.json atomically)
 *   - a plain JS object (legacy IAM-only JSON) — posts as
 *     `application/json`, routes to the v0.8.0 IAM-only path.
 */
export async function importBackup(
  data: Blob | File | Record<string, unknown>
): Promise<ImportBackupResult> {
  const isBlob = data instanceof Blob;
  const body = isBlob ? data : JSON.stringify(data);
  const contentType = isBlob ? 'application/zip' : 'application/json';
  const res = await fetch('/_/api/admin/backup', {
    method: 'POST',
    credentials: 'include',
    headers: { 'content-type': contentType },
    body,
  });
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(`Import failed: ${res.status}${text ? ` — ${text.slice(0, 200)}` : ''}`);
  }
  return res.json();
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

interface BackendListResponse {
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

interface RecoverDbResponse {
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

interface CreateMappingRuleRequest {
  provider_id?: number | null;
  priority?: number;
  match_type: string;
  match_field?: string;
  match_value: string;
  group_id: number;
}

interface UpdateMappingRuleRequest {
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

interface MappingPreviewResponse {
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

interface SyncResult {
  users_updated: number;
  memberships_changed: number;
}

export async function syncMemberships(): Promise<SyncResult> {
  const res = await adminFetch('/api/admin/ext-auth/sync-memberships', 'POST');
  if (!res.ok) throw new Error(`Sync failed: ${res.status}`);
  return safeJson(res);
}

// ─────────────────────────────────────────────────────────────
// Audit log (Wave 11 — Diagnostics → Audit panel)
// ─────────────────────────────────────────────────────────────

/**
 * One entry from the in-memory audit ring. Server-side type lives
 * in `src/audit.rs::AuditEntry` — keep this in sync if either side
 * adds fields.
 */
export interface AuditEntry {
  timestamp: string; // ISO-8601 UTC
  action: string;
  user: string;
  target: string;
  ip: string;
  ua: string;
  bucket: string;
  path: string;
}

interface AuditResponse {
  entries: AuditEntry[];
  limit: number;
}

/**
 * Fetch the most-recent `limit` audit entries (newest first). The
 * server caps `limit` at 500 regardless; the ring size itself is
 * governed by `DGP_AUDIT_RING_SIZE` (default 500).
 */
export async function fetchAudit(limit = 100): Promise<AuditResponse> {
  const res = await adminFetch(`/api/admin/audit?limit=${encodeURIComponent(limit)}`);
  if (!res.ok) throw new Error(`Audit fetch failed: ${res.status}`);
  return safeJson(res);
}
