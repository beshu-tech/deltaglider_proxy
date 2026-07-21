// === Multi-Backend Management ===
import { throwApiError } from '../errorHandling';
import { adminFetch, fetchJson, safeJson } from './core';
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
  return fetchJson('/api/admin/backends', 'Load backends');
}

export async function getBucketOrigins(): Promise<BucketOriginListResponse> {
  return fetchJson('/api/admin/buckets', 'Load bucket origins');
}

export async function createBucketOnBackend(
  name: string,
  backendName: string,
): Promise<{ success: boolean; bucket: string; backend_name: string }> {
  const res = await adminFetch('/api/admin/buckets', 'POST', {
    name,
    backend_name: backendName,
  });
  if (!res.ok) await throwApiError(res, `Create bucket ${name}`);
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

/** "Test connection": live connectivity/credentials probe of one backend;
 *  updates the server-side health cache and returns the fresh verdict. */
export async function probeBackend(
  name: string,
): Promise<import('./core').BackendHealthEntry> {
  const res = await adminFetch(`/api/admin/backends/${encodeURIComponent(name)}/probe`, 'POST');
  if (!res.ok) await throwApiError(res, `Test connection to ${name}`);
  return safeJson(res);
}
