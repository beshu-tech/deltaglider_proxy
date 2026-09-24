// Admin API client core: shared fetch glue + cross-cutting types.
import { throwApiError } from '../errorHandling';
import { BASE as APP_BASE } from '../urlState';

/** API path prefix: the SPA base without its trailing slash (`/_`). One
 *  source — `urlState.BASE` — so the app routes and the API cannot drift. */
export const BASE = APP_BASE.replace(/\/+$/, '');

/** A request body sent verbatim (not JSON-encoded), with its content type. */
interface RawBody {
  raw: BodyInit;
  contentType: string;
}

/**
 * Shared fetch wrapper — handles credentials, JSON serialization, content-type.
 * A `rawBody` is sent as-is instead of a JSON `body` (e.g. a backup zip).
 */
export async function adminFetch(
  path: string,
  method = 'GET',
  body?: unknown,
  rawBody?: RawBody,
): Promise<Response> {
  const opts: RequestInit = { method, credentials: 'include' };
  if (rawBody) {
    opts.headers = { 'Content-Type': rawBody.contentType };
    opts.body = rawBody.raw;
  } else if (body !== undefined) {
    opts.headers = { 'Content-Type': 'application/json' };
    opts.body = JSON.stringify(body);
  }
  return fetch(`${BASE}${path}`, opts);
}

interface AdminRequestOptions {
  method?: string;
  /** JSON-encoded request body. */
  body?: unknown;
  /** Operator-facing prefix of the error message ("Delete user 4 failed (409): …").
   *  Defaults to `API <path>`, the shape `safeJson` always produced. */
  context?: string;
}

function defaultContext(path: string): string {
  return `API ${BASE}${path.split('?')[0]}`;
}

/**
 * THE admin request helper: sends the request and throws an `ApiError`
 * (status + server detail, via `throwApiError`) on any non-2xx response.
 * Returns the raw `Response` for non-JSON bodies (text, blob, 204).
 */
export async function adminRequest(
  path: string,
  { method = 'GET', body, context }: AdminRequestOptions = {},
): Promise<Response> {
  const res = await adminFetch(path, method, body);
  if (!res.ok) await throwApiError(res, context ?? defaultContext(path));
  return res;
}

/** `adminRequest` + parse the JSON body. Use this for every JSON endpoint. */
export async function adminJson<T>(path: string, opts: AdminRequestOptions = {}): Promise<T> {
  return safeJson<T>(await adminRequest(path, opts));
}

/** @deprecated Use `adminJson(path, { context })`. Kept only for
 *  `bulkObjects.ts`, which another change owns. */
export async function fetchJson<T>(path: string, errorContext: string): Promise<T> {
  return adminJson<T>(path, { context: errorContext });
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
  // Best-effort: the response status is deliberately ignored. Logout must
  // never throw — the caller clears local credentials/session regardless, and
  // a failed server-side logout shouldn't trap the user in a logged-in UI.
  await adminFetch('/api/admin/logout', 'POST').catch(() => undefined);
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
      /** Omit / inherit; explicit override; JSON `null` on merge clears override (RFC 7396). */
      compression?: boolean | null;
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
      /** Declared replication destination: client writes 403; replication
       *  is the single writer (safe on non-CAS backends like B2). */
      replication_target_only?: boolean;
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

interface AdmissionMatch {
  method?: string[];
  source_ip?: string;
  source_ip_list?: string[];
  bucket?: string;
  path_glob?: string;
  authenticated?: boolean;
  config_flag?: string;
}

type AdmissionAction =
  | 'allow-anonymous'
  | 'deny'
  | 'continue'
  | { type: 'reject'; status: number; message?: string };

/**
 * Per-backend encryption status. Non-secret-only — the raw key never
 * leaves the server. Step 7 reads this to render the encryption
 * subsection in BackendsPanel and the per-bucket badge in BucketsPanel.
 *
 * `mode` values match the YAML wire tag:
 *   - `"none"` — plaintext.
 *   - `"aes256-gcm-proxy"` — proxy-side AES-256-GCM. `has_key` true
 *     iff the key material is actually loaded; `key_id` is the stable
 *     identifier stamped on every written object.
 *   - `"sse-kms"` — AWS KMS. `kms_key_id` is the ARN / alias.
 *   - `"sse-s3"` — AWS-managed AES256.
 *
 * `shim_active` is true when a `legacy_key` is configured — the
 * decrypt-only shim during a proxy→native mode transition. UI
 * surfaces this as an info banner reminding the operator to clear
 * `legacy_key` once all historical objects are gone.
 */
export type BackendEncryptionMode =
  | 'none'
  | 'aes256-gcm-proxy'
  | 'sse-kms'
  | 'sse-s3';

export interface BackendEncryptionSummary {
  mode: BackendEncryptionMode;
  has_key: boolean;
  key_id?: string;
  kms_key_id?: string;
  shim_active: boolean;
}

export interface BackendInfo {
  name: string;
  backend_type: string;
  path: string | null;
  endpoint: string | null;
  region: string | null;
  force_path_style: boolean | null;
  has_credentials: boolean;
  /**
   * Per-backend encryption status (Step 6/7 per-backend refactor).
   * Always present — the server synthesises a "default" entry for
   * the legacy singleton backend path so this field is uniform
   * across both YAML shapes.
   */
  encryption: BackendEncryptionSummary;
  /**
   * True when this entry was synthesised from the legacy singleton
   * `cfg.backend`. The server surfaces it so the Backends panel
   * doesn't claim "no named backends" while the proxy is actively
   * serving from the singleton. UI must:
   *   - disable Delete (no DB row to remove; the server returns 409
   *     on a DELETE of a synthesised name).
   *   - disable Encryption-mode changes that rely on a named target
   *     (mode transitions on the singleton still work, but the
   *     cleanest path is to migrate off the singleton first — add
   *     a named backend alongside, then clear `storage.backend`).
   *   - render a badge so operators understand what they're seeing.
   * Omitted in the response when false.
   */
  is_synthesized?: boolean;
  /**
   * Conditional-write verdict from the startup backend-capability gate
   * (guard B). Absent when the gate didn't assess this backend
   * (single-instance, or no client-writable routed buckets on it).
   */
  capability?: BackendCapabilityVerdict;
  /**
   * Connectivity/auth health from the boot probe + 30s re-probe loop.
   * Absent when probing is disabled (DGP_BOOT_BACKEND_PROBE=off).
   */
  health?: BackendHealthEntry;
}

/** Mirror of the server's coordination::CapabilityVerdict (kebab-case tag). */
type BackendCapabilityVerdict =
  | { verdict: 'cas-verified'; via: 'probe' }
  | { verdict: 'non-cas' }
  | { verdict: 'unknown'; reason: string };

/** Mirror of the server's coordination::health::HealthEntry (kebab-case tag). */
export type BackendHealthEntry = {
  /** Unix seconds of the probe that established this verdict. */
  probed_at: number;
} & (
  | { status: 'healthy' }
  | { status: 'auth-rejected'; detail: string }
  | { status: 'unreachable'; detail: string }
  | { status: 'erroring'; detail: string }
);

export async function getAdminConfig(): Promise<AdminConfig | null> {
  const res = await adminFetch('/api/admin/config');
  if (!res.ok) return null;
  return safeJson(res);
}

export async function checkSession(): Promise<{ valid: boolean; admin_gui: boolean }> {
  try {
    const res = await adminFetch('/api/admin/session');
    if (!res.ok) return { valid: false, admin_gui: false };
    const data = await safeJson<{ valid?: boolean; admin_gui?: boolean }>(res);
    return {
      valid: data.valid === true,
      admin_gui: data.admin_gui === true,
    };
  } catch {
    return { valid: false, admin_gui: false };
  }
}

interface ConfigUpdateResponse {
  success: boolean;
  warnings: string[];
  requires_restart: boolean;
}

/** Safely parse JSON from response, falling back to text for non-JSON content types. */
export async function safeJson<T>(res: Response): Promise<T> {
  if (!res.ok) {
    const path = (() => {
      try {
        return new URL(res.url).pathname;
      } catch {
        return 'request';
      }
    })();
    await throwApiError(res, `API ${path}`);
  }
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
  return adminJson('/api/admin/config', { method: 'PUT', body: updates });
}

/**
 * Dry-run a synthetic request through the live admission chain
 * (`POST /api/admin/config/trace`). Throws ApiError on a non-2xx, so
 * callers detect an expired session with `isSessionExpired`.
 */
export async function traceAdmission<T>(body: unknown): Promise<T> {
  const res = await adminFetch('/api/admin/config/trace', 'POST', body);
  if (!res.ok) await throwApiError(res, 'Trace');
  return safeJson<T>(res);
}

/**
 * Fetch the current runtime config as canonical YAML (four-section
 * shape, secrets redacted). Backs the "Copy as YAML" / "Export"
 * button flows. Returns the raw YAML string — the UI renders it
 * syntax-highlighted in a modal.
 */
export async function exportConfigYaml(): Promise<string> {
  const res = await adminRequest('/api/admin/config/export', { context: 'Config export' });
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
  return adminJson('/api/admin/config/validate', { method: 'POST', body: { yaml } });
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
  return adminJson('/api/admin/config/apply', { method: 'POST', body: { yaml } });
}

// ═══════════════════════════════════════════════════════════════════
// Full-IAM YAML export / import.
//
// Distinct from the runtime-config YAML above: this round-trips the
// ENTIRE IAM state (users, groups, OAuth providers, group-mapping
// rules) as an `access:`-shaped declarative YAML. Export includes
// REAL secrets (secret_access_key, client_secret) so the round-trip
// is lossless — the file holds live credentials, so the UI warns
// prominently. Import is validate (dry-run diff) → confirm → apply.
// ═══════════════════════════════════════════════════════════════════

/**
 * Export the full IAM state as declarative YAML. With
 * `includeSecrets` (the default for this flow), the document carries
 * real `secret_access_key` / `client_secret` values so a re-import
 * is lossless. ⚠️ The returned text contains LIVE credentials.
 */
export async function exportFullIamYaml(includeSecrets = true): Promise<string> {
  const qs = includeSecrets ? '?include_secrets=true' : '';
  const res = await adminRequest(`/api/admin/config/declarative-iam-export${qs}`, {
    context: 'Full IAM export',
  });
  return res.text();
}

/** Per-category change counts returned by the IAM import dry-run and apply. */
export interface IamImportSummary {
  users_created: number;
  users_updated: number;
  users_deleted: number;
  groups_created: number;
  groups_updated: number;
  groups_deleted: number;
  providers_created: number;
  providers_updated: number;
  providers_deleted: number;
  mapping_rules_replaced: number;
  no_changes: boolean;
}

/**
 * Dry-run a full-IAM YAML import: parse + diff against the live DB,
 * returning the change summary WITHOUT mutating any state. Powers the
 * confirm-before-apply preview.
 */
export async function validateFullIamYaml(yaml: string): Promise<IamImportSummary> {
  return adminJson('/api/admin/config/declarative-iam-validate', {
    method: 'POST',
    body: { yaml },
    context: 'Full IAM import validation',
  });
}

/**
 * Apply a full-IAM YAML import: reconcile the entire IAM set
 * atomically (single SQLite transaction), rebuild the index, sync to
 * peers, and audit each mutation. Returns the applied change summary.
 */
export async function applyFullIamYaml(yaml: string): Promise<IamImportSummary> {
  return adminJson('/api/admin/config/declarative-iam-apply', {
    method: 'POST',
    body: { yaml },
    context: 'Full IAM import apply',
  });
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
  return adminJson(`/api/admin/config/section/${section}`, {
    context: `Section fetch (${section})`,
  });
}

/**
 * Fetch one section as canonical YAML text (the `?format=yaml`
 * variant). Backs the per-section Copy-as-YAML button.
 */
export async function getSectionYaml(section: SectionName): Promise<string> {
  const res = await adminRequest(`/api/admin/config/section/${section}?format=yaml`, {
    context: `Section YAML fetch (${section})`,
  });
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
  return adminJson(`/api/admin/config/section/${section}`, { method: 'PUT', body });
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
  return adminJson(`/api/admin/config/section/${section}/validate`, { method: 'POST', body });
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
  return adminJson('/api/admin/test-s3', { method: 'POST', body: req });
}

export async function changeAdminPassword(
  currentPassword: string,
  newPassword: string
): Promise<PasswordChangeResponse> {
  return adminJson('/api/admin/password', {
    method: 'PUT',
    body: { current_password: currentPassword, new_password: newPassword },
  });
}
