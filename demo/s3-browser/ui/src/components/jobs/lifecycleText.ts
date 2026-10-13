/**
 * Pure text for the lifecycle preview, the run-now confirmation and the
 * rule editor. No React: unit-tested in `__tests__/lifecycleText.test.ts`.
 */

/** What a preview candidate will do, in the words the operator decides on. */
export function lifecycleActionLabel(c: {
  action: string;
  destination_bucket?: string;
  destination_key?: string;
  delete_source_after_success: boolean;
}): string {
  if (c.action !== 'transition') return c.action;
  const verb = c.delete_source_after_success ? 'move to' : 'copy to';
  const dest = c.destination_key
    ? `${c.destination_bucket ?? '?'}/${c.destination_key}`
    : c.destination_bucket ?? '?';
  return `${verb} ${dest}`;
}

function n(v: number): string {
  return v.toLocaleString('en-US');
}

/** "12 are not old enough yet, 3 are kept out by a glob." or null. */
export function lifecycleSkipText(p: { objects_not_due?: number; objects_excluded?: number }): string | null {
  const parts: string[] = [];
  const notDue = p.objects_not_due ?? 0;
  const excluded = p.objects_excluded ?? 0;
  if (notDue > 0) parts.push(`${n(notDue)} ${notDue === 1 ? 'is' : 'are'} not old enough yet`);
  if (excluded > 0) parts.push(`${n(excluded)} ${excluded === 1 ? 'is' : 'are'} kept out by a glob`);
  if (parts.length === 0) return null;
  return `Of the other scanned objects, ${parts.join(', ')}.`;
}

/** "delete 46, move 3" — what a preview or a run does, by kind of action. */
export function lifecycleActionCounts(p: {
  objects_deleted?: number;
  objects_moved?: number;
  objects_copied?: number;
}): string {
  const parts: string[] = [];
  if (p.objects_deleted) parts.push(`delete ${n(p.objects_deleted)}`);
  if (p.objects_moved) parts.push(`move ${n(p.objects_moved)}`);
  if (p.objects_copied) parts.push(`copy ${n(p.objects_copied)}`);
  return parts.join(', ');
}

/** How long ago the proxy walked the scope, e.g. "8 s ago", "3 min ago". */
export function previewAgeText(computedAtMs: number, nowMs: number): string {
  const s = Math.max(0, Math.round((nowMs - computedAtMs) / 1000));
  if (s < 60) return `${s} s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  return `${Math.round(m / 60)} h ago`;
}

/**
 * The run-now confirmation shows the count that the run will act on, so it
 * asks the proxy for a new walk when the preview it has is older than this.
 */
export const CONFIRM_MAX_PREVIEW_AGE_MS = 10_000;

/** Must the confirmation ask for a fresh walk? (No preview, or an old one.) */
export function confirmNeedsFreshPreview(computedAt: string | undefined, nowMs: number): boolean {
  if (!computedAt) return true;
  const at = Date.parse(computedAt);
  return Number.isNaN(at) || nowMs - at > CONFIRM_MAX_PREVIEW_AGE_MS;
}

/** The terminal statuses of a lifecycle run. */
export function isSettledRunStatus(status: string): boolean {
  return !['running', 'queued', 'cancelling'].includes(status);
}

/** The toast after a run-now settles. */
export function lifecycleRunResultText(
  rule: string,
  run: {
    status: string;
    objects_deleted?: number | null;
    objects_moved?: number;
    objects_copied?: number;
    objects_processed: number;
    errors: number;
  },
): { type: 'success' | 'warning' | 'error'; text: string } {
  const parts: string[] = [];
  if (run.objects_deleted) parts.push(`deleted ${n(run.objects_deleted)}`);
  if (run.objects_moved) parts.push(`moved ${n(run.objects_moved)}`);
  if (run.objects_copied) parts.push(`copied ${n(run.objects_copied)}`);
  // A run from before the split was stored: only the total is known.
  if (parts.length === 0 && run.objects_processed > 0) parts.push(`acted on ${n(run.objects_processed)}`);
  const did = parts.length > 0 ? parts.join(', ') : 'nothing to do';
  const failed = run.errors > 0 ? `, ${n(run.errors)} failed` : '';
  if (run.status === 'paused') return { type: 'warning', text: `Lifecycle rule "${rule}" paused: ${did}${failed}.` };
  if (run.status === 'cancelled') return { type: 'warning', text: `Lifecycle rule "${rule}" killed: ${did}${failed}.` };
  if (run.status === 'failed') return { type: 'error', text: `Lifecycle rule "${rule}" failed: ${did}${failed}.` };
  return { type: run.errors > 0 ? 'warning' : 'success', text: `Lifecycle rule "${rule}" finished: ${did}${failed}.` };
}

/**
 * Why a glob pattern is invalid, or null. Mirrors the cases the proxy's
 * glob parser refuses: an unclosed `[` class or `{` alternation, and a
 * stray closing `}`. The proxy stays the judge; this marks the field
 * before the apply.
 */
export function globError(pattern: string): string | null {
  let inClass = false;
  let braces = 0;
  for (let i = 0; i < pattern.length; i += 1) {
    const ch = pattern[i];
    if (ch === '\\') {
      i += 1;
      continue;
    }
    if (inClass) {
      if (ch === ']' && pattern[i - 1] !== '[' && !(pattern[i - 1] === '!' && pattern[i - 2] === '[')) inClass = false;
      continue;
    }
    if (ch === '[') inClass = true;
    else if (ch === '{') braces += 1;
    else if (ch === '}') {
      if (braces === 0) return `"${pattern}" has a "}" without a "{"`;
      braces -= 1;
    }
  }
  if (inClass) return `"${pattern}" has a "[" without a "]"`;
  if (braces > 0) return `"${pattern}" has a "{" without a "}"`;
  return null;
}

/** The first invalid glob of a rule, as a field error that names the rule. */
export function ruleGlobError(rule: string, globs: readonly string[]): string | null {
  for (const g of globs) {
    const why = globError(g);
    if (why) return `Rule "${rule}": ${why}. The proxy refuses this rule until you fix the glob.`;
  }
  return null;
}

/**
 * When a NEW enabled rule first runs on schedule: the proxy schedules it one
 * tick after it first sees the rule (the apply). `tickMs` from the tick
 * interval; null when the tick does not parse.
 */
export function firstRunText(tickInterval: string, nowMs: number): string | null {
  const tickMs = parseTickMs(tickInterval);
  if (tickMs == null) return null;
  const at = new Date(nowMs + tickMs);
  return `first scheduled run about ${tickInterval} after you apply (at about ${at.toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
  })})`;
}

const UNIT_MS: Record<string, number> = {
  s: 1000,
  sec: 1000,
  m: 60_000,
  min: 60_000,
  h: 3_600_000,
  hr: 3_600_000,
  d: 86_400_000,
};

/** "1h", "30m", "2h30m", "1d" → ms (the humantime forms the YAML uses). Below
 *  the proxy's 60 s minimum tick → 60 s. */
export function parseTickMs(tick: string): number | null {
  const re = /(\d+)\s*([a-z]+)/g;
  let total = 0;
  let matched = '';
  for (const m of tick.trim().matchAll(re)) {
    const unit = UNIT_MS[m[2]];
    if (!unit) return null;
    total += Number(m[1]) * unit;
    matched += m[0];
  }
  if (!matched || matched.replace(/\s/g, '') !== tick.replace(/\s/g, '')) return null;
  return Math.max(total, 60_000);
}
