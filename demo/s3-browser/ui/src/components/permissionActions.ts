/**
 * Pure model + helpers for the "CAN DO" action chips (see ActionChips.tsx).
 *
 * The IAM action set is {read, write, delete, list, admin} — a SET, not a
 * cumulative ladder, so "write without delete" is expressible. The wildcard
 * `*` is the collapsed form of all five. These helpers keep the toggle logic
 * (expand `*`, collapse-to-`*`, canonical ordering) out of the component so it
 * is unit-testable without React. No React/antd imports.
 */

import { parseResourcePattern } from '../storagePath';

/** Canonical atomic-action order (drives chip order + serialized order). */
const ATOMIC_ACTIONS = ['list', 'read', 'write', 'delete', 'admin'] as const;

/**
 * True when EVERY concrete resource in the rule is scoped to a sub-prefix
 * (`bucket/prefix/*`) rather than the whole bucket (`bucket`, `bucket/*`) or
 * global `*`. Drives whether the Admin chip (bucket-level Create/DeleteBucket)
 * is offered: admin is meaningless on a sub-prefix, so we suppress it there.
 *
 * Decisions (locked by the regression test):
 * - Bucket-only (`bucket`, `bucket/*`), global `*`, and empty string → false
 *   (admin offered / N/A).
 * - A MIXED list where ANY resource is bucket-level → false (admin meaningful
 *   for that one, so offer it; `.every` short-circuits).
 * - A template-bucket resource (`${iam:username}/x/*`) WITH a prefix → true. A
 *   templated bucket is still not a real bucket-op target, and failing toward
 *   "admin hidden" is the privilege-safe choice. (This is the one spot the
 *   template case is intentionally treated like any other prefix grant —
 *   `${iam:username}/*` with no prefix → false, admin offered, same as a bare
 *   bucket.)
 */
export function isPrefixScoped(resources: string[]): boolean {
  const parts = resources.map((p) => p.trim()).filter(Boolean);
  if (parts.length === 0) return false;
  return parts.every((p) => {
    if (p === '*') return false;
    const { prefix, global } = parseResourcePattern(p);
    return !global && prefix.length > 0;
  });
}

/** Expand a stored action list to the concrete set of atomic actions held. */
export function effectiveActions(actions: string[]): Set<string> {
  if (actions.includes('*')) return new Set(ATOMIC_ACTIONS);
  return new Set(actions.filter((a) => (ATOMIC_ACTIONS as readonly string[]).includes(a)));
}

/**
 * Toggle one atomic action against the current list, returning the next list.
 * Collapses to `['*']` when all five are present (compact wire form,
 * `is_admin`-detectable); expands `*` to explicit actions on first removal.
 * Output is always in canonical {@link ATOMIC_ACTIONS} order.
 */
export function toggleAction(actions: string[], value: string): string[] {
  const set = effectiveActions(actions);
  if (set.has(value)) set.delete(value);
  else set.add(value);
  if (set.size === ATOMIC_ACTIONS.length) return ['*'];
  return ATOMIC_ACTIONS.filter((a) => set.has(a));
}

/**
 * Reconcile the action set to a (possibly changed) scope: `admin` is only
 * meaningful at bucket scope, so when the grant becomes prefix-scoped we must
 * STRIP it — otherwise narrowing the resource after granting Admin leaves a
 * stale `admin` (or a `*` that still implies admin) that the disabled chip
 * can no longer represent. Returns the input unchanged when nothing needs
 * dropping (referential stability for callers that diff). At bucket scope it's
 * a no-op. Output stays in canonical order; a collapsed `*` is expanded to the
 * four non-admin actions when admin must go.
 */
export function reconcileActionsForScope(actions: string[], prefixScoped: boolean): string[] {
  if (!prefixScoped) return actions;
  const set = effectiveActions(actions);
  if (!set.has('admin')) return actions;
  set.delete('admin');
  return ATOMIC_ACTIONS.filter((a) => set.has(a));
}

/**
 * A short plain-language description of what a grant's action set allows,
 * mirroring the mockup caption (e.g. "Browse, download & upload. Grants
 * list, read, write."). Returns a guidance string for the empty set.
 */
export function grantSummary(actions: string[]): string {
  const held = effectiveActions(actions);
  if (held.size === 0) return 'No actions selected — this grant does nothing.';
  if (held.size === ATOMIC_ACTIONS.length) {
    return 'Full control, including bucket-level operations. Grants *.';
  }
  const verbs: string[] = [];
  if (held.has('list')) verbs.push('browse');
  if (held.has('read')) verbs.push('download');
  if (held.has('write')) verbs.push('upload');
  if (held.has('delete')) verbs.push('delete');
  if (held.has('admin')) verbs.push('manage buckets');
  const human = verbs.join(', ').replace(/, ([^,]*)$/, ' & $1');
  const grants = ATOMIC_ACTIONS.filter((a) => held.has(a)).join(', ');
  const cap = human.charAt(0).toUpperCase() + human.slice(1);
  return `${cap}. Grants ${grants}.`;
}
