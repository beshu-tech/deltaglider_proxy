// === Whoami / Login-as ===
import { ApiError } from '../errorHandling';
import { adminFetch, adminRequest, safeJson } from './core';
import type { IamPermission } from './users';

export interface ExternalProviderInfo {
  name: string;
  type: string;
  display_name: string;
}

export interface WhoamiResponse {
  mode: 'bootstrap' | 'iam' | 'open';
  /** Running proxy version — only present when the request carried a live session. */
  version?: string;
  /** UTC build timestamp of the running binary — same gate as `version`. */
  build_time?: string;
  user: { name: string; access_key_id: string; is_admin: boolean; permissions?: IamPermission[] } | null;
  /** How the session signed in; absent without a session (or on servers before 1.19). */
  auth_method?: 'bootstrap' | 'iam' | 'iam_browser' | 'open' | 'external';
  config_db_mismatch?: boolean;
  /** Typed lock signal from the server: 'locked' when the config DB is
   *  bootstrap-hash-mismatched. Prefer this over inferring from error text. */
  lock_state?: 'locked';
  external_providers?: ExternalProviderInfo[];
}

/** Single source of truth for "is the config DB locked": typed signal first,
 *  the older bool kept only as a fallback for servers predating lock_state. */
export function isConfigDbLocked(info: WhoamiResponse): boolean {
  return info.lock_state === 'locked' || info.config_db_mismatch === true;
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

export async function resolveIamIdentity(accessKeyId: string, secretAccessKey: string): Promise<WhoamiResponse | null> {
  try {
    const res = await adminFetch('/api/iam/identity', 'POST', {
      access_key_id: accessKeyId,
      secret_access_key: secretAccessKey,
    });
    if (!res.ok) return null;
    return await safeJson(res);
  } catch (err) {
    console.warn('IAM identity resolve failed:', err);
    return null;
  }
}

export type LoginAsResult = { ok: true } | { ok: false; status: number; error: string };

/**
 * IAM admin sign-in with S3 keys. On failure, `status` tells the caller WHY:
 * 403 is the server's single answer for "unknown key, wrong secret, or not an
 * admin"; 429 (rate limit) and 5xx are not an answer about the user at all.
 */
export async function loginAs(accessKeyId: string, secretAccessKey: string): Promise<LoginAsResult> {
  try {
    await adminRequest('/api/admin/login-as', {
      method: 'POST',
      body: { access_key_id: accessKeyId, secret_access_key: secretAccessKey },
      context: 'Admin sign-in',
    });
    return { ok: true };
  } catch (e) {
    if (!(e instanceof ApiError)) throw e; // network failure: the caller shows it
    if (e.status === 403) {
      return {
        ok: false,
        status: 403,
        error: 'Admin access denied — invalid credentials or insufficient permissions',
      };
    }
    return { ok: false, status: e.status, error: e.message };
  }
}

/** THE rule for "fall back to a files-only session": login-as said 403. A rate
 *  limit or a server error must be shown, never silently downgraded. */
export function isNotAdminDenial(result: LoginAsResult): boolean {
  return !result.ok && result.status === 403;
}

/** IAM non-admin: cookie + server-stored S3 creds (survives hard refresh). */
export async function browserSessionConnect(req: {
  access_key_id: string;
  secret_access_key: string;
  endpoint: string;
  region?: string;
  bucket?: string;
}): Promise<{ ok: boolean; error?: string }> {
  const res = await adminFetch('/api/admin/session/browser-connect', 'POST', {
    access_key_id: req.access_key_id,
    secret_access_key: req.secret_access_key,
    endpoint: req.endpoint,
    region: req.region,
    bucket: req.bucket ?? '',
  });
  if (res.ok) return { ok: true };
  let error = res.status === 429 ? 'Too many attempts' : 'Could not create browser session';
  try {
    const data = (await res.json()) as { error?: string };
    if (data?.error) error = data.error;
  } catch {
    /* keep generic */
  }
  return { ok: false, error };
}

/** Open auth mode only: cookie + anonymous S3 creds for hard refresh. */
export async function openBrowserConnect(req: {
  endpoint: string;
  region?: string;
  bucket?: string;
}): Promise<{ ok: boolean; error?: string }> {
  const res = await adminFetch('/api/admin/session/open-browser-connect', 'POST', {
    endpoint: req.endpoint,
    region: req.region,
    bucket: req.bucket ?? '',
  });
  if (res.ok) return { ok: true };
  let error = res.status === 429 ? 'Too many attempts' : 'Could not start open browser session';
  try {
    const data = (await res.json()) as { error?: string };
    if (data?.error) error = data.error;
  } catch {
    /* keep generic */
  }
  return { ok: false, error };
}
