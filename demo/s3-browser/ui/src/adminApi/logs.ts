// ─────────────────────────────────────────────────────────────
// Operational logs (Diagnostics → Logs panel) — backlog + live tail.
// Server-side types: src/logs.rs::LogEntry. Keep in sync.
// ─────────────────────────────────────────────────────────────
import { fetchJson, BASE } from './core';

export interface LogEntry {
  ts: string; // ISO-8601 UTC
  level: string; // ERROR | WARN | INFO | DEBUG | TRACE
  target: string; // module path
  message: string;
  fields: Record<string, unknown>;
}

interface LogsResponse {
  entries: LogEntry[];
  limit: number;
}

export interface LogFilters {
  level?: string;
  target?: string;
  q?: string;
}

function filterParams(f: LogFilters): URLSearchParams {
  const p = new URLSearchParams();
  if (f.level) p.set('level', f.level);
  if (f.target) p.set('target', f.target);
  if (f.q) p.set('q', f.q);
  return p;
}

/** Recent backlog (newest first), server-side filtered. */
export async function fetchLogs(filters: LogFilters = {}, limit = 200): Promise<LogsResponse> {
  const p = filterParams(filters);
  p.set('limit', String(limit));
  return fetchJson(`/api/admin/logs?${p}`, 'Logs fetch');
}

/**
 * Live tail via SSE. Calls `onEntry` per new log line (matching the same
 * server-side filter), `onError` on stream failure. Returns an unsubscribe
 * function that closes the stream. Uses the native EventSource (no dep);
 * cookies ride along because it's same-origin.
 */
export function streamLogs(
  filters: LogFilters,
  onEntry: (entry: LogEntry) => void,
  onLagged?: (msg: string) => void,
  onError?: () => void,
): () => void {
  const p = filterParams(filters);
  const es = new EventSource(`${BASE}/api/admin/logs/stream?${p}`, { withCredentials: true });

  es.addEventListener('log', (ev) => {
    try {
      onEntry(JSON.parse((ev as MessageEvent).data) as LogEntry);
    } catch {
      /* ignore malformed frame */
    }
  });
  es.addEventListener('lagged', (ev) => {
    onLagged?.((ev as MessageEvent).data as string);
  });
  es.onerror = () => {
    // EventSource auto-reconnects; surface the blip but don't tear down unless
    // the caller wants to. Closing here would disable the built-in retry.
    onError?.();
  };

  return () => es.close();
}
