// ─────────────────────────────────────────────────────────────
// Audit log (Wave 11 — Diagnostics → Audit panel)
// ─────────────────────────────────────────────────────────────
import { adminJson } from './core';

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
  return adminJson(`/api/admin/audit?limit=${encodeURIComponent(limit)}`, {
    context: 'Audit fetch',
  });
}
