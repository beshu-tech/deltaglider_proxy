/**
 * Pure: live sessions grouped by principal for SessionsPanel (UX harvest
 * D55). One group per signed-in principal: its sessions newest first, and
 * the groups in the order of their newest session. The caller's own group
 * comes first.
 */
import type { SessionSummary } from '../adminApi';

export interface SessionGroup {
  key: string;
  /** What the admin reads: the access key, provider:user, or a role name. */
  label: string;
  /** How the principal signs in, in words: "IAM user", "External provider"… */
  method: string;
  /** Newest first. */
  sessions: SessionSummary[];
  /** Sessions that a click can end: every session but the caller's own. */
  others: SessionSummary[];
  hasCurrent: boolean;
}

const METHOD: Record<string, string> = {
  bootstrap: 'Bootstrap password',
  iam: 'IAM user',
  iam_browser: 'IAM user, file browser',
  external: 'External provider',
  open: 'Open access',
};

function principal(s: SessionSummary): { key: string; label: string } {
  if (s.identity) return { key: `id:${s.identity}`, label: s.identity };
  if (s.auth === 'bootstrap') return { key: 'bootstrap', label: 'Bootstrap admin' };
  if (s.auth === 'open') return { key: 'open', label: 'Anonymous (open access)' };
  return { key: `auth:${s.auth}`, label: s.auth };
}

export function groupSessions(rows: readonly SessionSummary[]): SessionGroup[] {
  const groups = new Map<string, SessionGroup>();
  for (const s of rows) {
    const { key, label } = principal(s);
    let g = groups.get(key);
    if (!g) {
      g = { key, label, method: METHOD[s.auth] ?? s.auth, sessions: [], others: [], hasCurrent: false };
      groups.set(key, g);
    }
    g.sessions.push(s);
    if (s.current) g.hasCurrent = true;
    else g.others.push(s);
  }
  const out = [...groups.values()];
  for (const g of out) {
    g.sessions.sort((a, b) => a.age_secs - b.age_secs);
    g.others.sort((a, b) => a.age_secs - b.age_secs);
  }
  return out.sort((a, b) => Number(b.hasCurrent) - Number(a.hasCurrent) || a.sessions[0].age_secs - b.sessions[0].age_secs);
}

/** A short form of a session id for reading; the full id stays in titles and labels. */
export function shortSessionId(id: string): string {
  return id.length > 6 ? id.slice(0, 6) : id;
}
