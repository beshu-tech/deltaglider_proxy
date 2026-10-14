// === Multi-Backend Management ===
import { adminJson } from './core';
import type { BackendEncryptionMode, BackendInfo } from './core';

interface BackendListResponse {
  backends: BackendInfo[];
  default_backend: string | null;
}

interface BucketOriginResponse {
  name: string;
  creation_date: string | null;
  backend_name?: string | null;
  backend_type?: string | null;
  backend_endpoint?: string | null;
  backend_region?: string | null;
  backend_path?: string | null;
  real_bucket?: string | null;
  /** Verbatim backend error when this bucket's backend couldn't be listed (503/throttle). */
  unavailable?: string | null;
  /** The server's answer to "how are this bucket's new writes encrypted".
   *  null only when the bucket routes to an undefined backend. */
  effective_encryption?: EffectiveEncryption | null;
}

/**
 * `effective_encryption` of one bucket (issue #116), computed by the
 * server's one resolver. The UI shows it; it never resolves a bucket's
 * encryption from the backend list itself (source guard
 * `noClientEncryptionResolution.test.ts`).
 */
export interface EffectiveEncryption {
  mode: BackendEncryptionMode;
  /** `bucket`: the bucket's own block. `backend`: inherited from `backend`. */
  source: 'bucket' | 'backend';
  /** The raw backend that stores the bucket. */
  backend: string;
  has_key: boolean;
  key_id?: string;
  /** Decrypt-only legacy key id of the site (the bucket's or the backend's). */
  legacy_key_id?: string;
  kms_key_id?: string;
  /** The bucket's own key comes from this environment variable. */
  key_env?: string;
  /** New writes are stored encrypted. */
  encrypts: boolean;
  /** A re-encrypt job can change objects of this bucket. */
  reencrypt: boolean;
  /** The config fixes the bucket's backend; an edit that adds the bucket's
   *  own block must also set `backend:` when false. */
  pinned: boolean;
  /** The modes that the bucket's own block accepts. */
  bucket_modes: BackendEncryptionMode[];
  /** What "Inherit" gives the bucket: the backend's own encryption. */
  inherited: { mode: BackendEncryptionMode; encrypts: boolean; key_id?: string };
}

interface BucketOriginListResponse {
  buckets: BucketOriginResponse[];
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
  return adminJson('/api/admin/backends', { context: 'Load backends' });
}

export async function getBucketOrigins(): Promise<BucketOriginListResponse> {
  return adminJson('/api/admin/buckets', { context: 'Load bucket origins' });
}

export async function createBucketOnBackend(
  name: string,
  backendName: string,
): Promise<{ success: boolean; bucket: string; backend_name: string }> {
  return adminJson('/api/admin/buckets', {
    method: 'POST',
    body: { name, backend_name: backendName },
    context: `Create bucket ${name}`,
  });
}



export async function createBackend(req: CreateBackendRequest): Promise<{ success: boolean; error?: string }> {
  return adminJson('/api/admin/backends', { method: 'POST', body: req });
}

export async function deleteBackend(name: string): Promise<{ success: boolean; error?: string }> {
  return adminJson(`/api/admin/backends/${encodeURIComponent(name)}`, { method: 'DELETE' });
}

/** "Test connection": live connectivity/credentials probe of one backend;
 *  updates the server-side health cache and returns the fresh verdict. */
export async function probeBackend(
  name: string,
): Promise<import('./core').BackendHealthEntry> {
  return adminJson(`/api/admin/backends/${encodeURIComponent(name)}/probe`, {
    method: 'POST',
    context: `Test connection to ${name}`,
  });
}

/**
 * `GET /backends/:name/legacy-key-usage`: how many objects and delta
 * references still carry the legacy key id. Exact up to `limit` objects;
 * past that the scan stops and `complete` is false.
 */
export interface LegacyKeyUsage {
  backend: string;
  /** Set when the check was for one bucket's own legacy key. */
  bucket?: string | null;
  legacy_key_id: string | null;
  buckets: string[];
  objects_scanned: number;
  objects_under_legacy_key: number;
  references_scanned: number;
  references_under_legacy_key: number;
  examples: string[];
  errors: string[];
  complete: boolean;
  limit: number;
  safe_to_clear: boolean;
}

/**
 * Objects one check reads. The server default is 10,000 (max 1,000,000); a
 * check of 20,000 objects on a local MinIO answers in about 1 s, so the UI
 * asks for more than the default and stays well under a minute on a remote
 * backend.
 */
export const LEGACY_KEY_SCAN_LIMIT = 50_000;

/** `fresh` skips the proxy's 60 s cache of the last check. */
export async function getLegacyKeyUsage(
  name: string,
  fresh = false,
  /** Scope the check to this bucket's own legacy key id. */
  bucket?: string,
): Promise<LegacyKeyUsage> {
  const query = `?limit=${LEGACY_KEY_SCAN_LIMIT}${fresh ? '&fresh=true' : ''}${
    bucket ? `&bucket=${encodeURIComponent(bucket)}` : ''
  }`;
  return adminJson(`/api/admin/backends/${encodeURIComponent(name)}/legacy-key-usage${query}`, {
    context: bucket ? `Check the legacy key of bucket ${bucket}` : `Check the legacy key of ${name}`,
  });
}
