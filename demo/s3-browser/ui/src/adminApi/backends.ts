// === Multi-Backend Management ===
import { adminJson } from './core';
import type { BackendInfo } from './core';

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

export async function getLegacyKeyUsage(name: string): Promise<LegacyKeyUsage> {
  return adminJson(`/api/admin/backends/${encodeURIComponent(name)}/legacy-key-usage`, {
    context: `Check the legacy key of ${name}`,
  });
}
