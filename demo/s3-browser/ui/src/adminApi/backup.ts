// === Full Backup / Restore ===
//
// Since v0.8.4 the default shape is a zip containing config.yaml +
// iam.json + secrets.json + manifest.json. The legacy IAM-only JSON
// export stays addressable via `?format=json` for backwards compat,
// but every admin GUI flow uses the zip exclusively.
import { adminFetch, adminRequest } from './core';

/**
 * Download the Full Backup as a zip Blob. Callers pipe this into a
 * File-Saver-style `<a download>` dance; the caller owns the saved
 * filename (typically derived from the Content-Disposition header).
 */
export async function exportBackup(): Promise<{ blob: Blob; filename: string }> {
  const res = await adminRequest('/api/admin/backup', { context: 'Export' });
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
  users_deleted?: number;
  groups_created: number;
  groups_deleted?: number;
  groups_skipped: number;
  memberships_created: number;
  external_identities_created?: number;
  external_identities_skipped?: number;
}

export type ImportBackupMode = 'full' | 'preserve-bootstrap' | 'iam-only' | 'config-only';

/**
 * How a restore treats users, groups and OIDC providers that exist now:
 * `replace` (point-in-time: what the backup lacks is deleted) or `merge`
 * (what the backup lacks is kept; existing entries are not overwritten).
 */
export type ImportIamMode = 'replace' | 'merge';

interface ImportBackupErrorBody {
  error?: string;
  stage?: string;
  context?: string;
  detail?: string;
  upstream_status?: number;
}

export class ImportBackupError extends Error {
  status: number;
  response?: ImportBackupErrorBody;
  rawBody?: string;

  constructor(status: number, response?: ImportBackupErrorBody, rawBody?: string) {
    const detail =
      response?.error ||
      [response?.stage, response?.context, response?.detail].filter(Boolean).join(': ') ||
      rawBody ||
      'backup import failed';
    const upstream = response?.upstream_status ? ` (upstream ${response.upstream_status})` : '';
    super(`Import failed: ${status}${upstream} — ${detail.slice(0, 700)}`);
    this.name = 'ImportBackupError';
    this.status = status;
    this.response = response;
    this.rawBody = rawBody;
  }
}

async function parseImportBackupError(res: Response): Promise<ImportBackupError> {
  const text = await res.text().catch(() => '');
  try {
    const parsed = text ? (JSON.parse(text) as ImportBackupErrorBody) : undefined;
    return new ImportBackupError(res.status, parsed, text);
  } catch {
    return new ImportBackupError(res.status, undefined, text);
  }
}

/**
 * Restore from a backup file. Accepts either:
 *   - a `File` / `Blob` of a zip exported by this server (posts as
 *     `application/zip`, goes through the scoped zip import path
 *     selected by `mode`)
 *   - a plain JS object (legacy IAM-only JSON) — posts as
 *     `application/json`, routes to the v0.8.0 IAM-only path.
 */
export async function importBackup(
  data: Blob | File | Record<string, unknown>,
  mode: ImportBackupMode = 'full',
  iam: ImportIamMode = 'replace'
): Promise<ImportBackupResult> {
  const isBlob = data instanceof Blob;
  const body = isBlob ? data : JSON.stringify(data);
  const contentType = isBlob ? 'application/zip' : 'application/json';
  const qs = new URLSearchParams({ mode, iam });
  const res = await adminFetch(`/api/admin/backup?${qs.toString()}`, 'POST', undefined, {
    raw: body,
    contentType,
  });
  if (!res.ok) {
    const err = await parseImportBackupError(res);
    console.error('Backup import failed', {
      status: err.status,
      response: err.response,
      rawBody: err.rawBody,
    });
    throw err;
  }
  return res.json();
}
