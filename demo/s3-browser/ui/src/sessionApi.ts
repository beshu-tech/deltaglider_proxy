// Server-side session credential API.
// Credentials are stored in an httpOnly session cookie on the server —
// never in localStorage — to prevent XSS exfiltration.

import { adminFetch } from './adminApi';

export interface SessionS3Credentials {
  endpoint: string;
  region: string;
  bucket: string;
  access_key_id: string;
  secret_access_key: string;
}

/** Fetch S3 credentials stored in the current server-side session. */
export async function fetchSessionCredentials(): Promise<SessionS3Credentials | null> {
  try {
    const res = await adminFetch('/api/admin/session/s3-credentials');
    if (!res.ok) return null;
    return await res.json();
  } catch {
    return null;
  }
}

/** Store or update S3 credentials in the server-side session. */
export async function storeSessionCredentials(creds: SessionS3Credentials): Promise<boolean> {
  try {
    const res = await adminFetch('/api/admin/session/s3-credentials', 'PUT', creds);
    return res.ok;
  } catch {
    return false;
  }
}

/** Clear S3 credentials from the server-side session (disconnect). */
export async function clearSessionCredentials(): Promise<void> {
  try {
    await adminFetch('/api/admin/session/s3-credentials', 'DELETE');
  } catch {
    // Best-effort
  }
}
