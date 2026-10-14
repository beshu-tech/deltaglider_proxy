/**
 * A stable client-side identity for each rule in a rule editor (replication,
 * lifecycle). A rule's NAME is not an identity while the operator edits:
 * typing a new rule's name passes through other names ("nightly" on the way
 * to "nightly-eu"), and a cleared name is empty. The editor finds, opens and
 * removes a rule by this uid instead.
 *
 * The uid sits under a Symbol key: object spread (`{ ...rule, ...patch }`)
 * copies it, and JSON.stringify drops it. So it never reaches the YAML, the
 * PUT body, or the editor's dirty comparison (`jsonEq`).
 */
const UID: unique symbol = Symbol('ruleUid');
type Tagged = { [UID]?: string };

let counter = 0;
/** Rules that came in without a uid (tests, old callers): one per object. */
const untagged = new WeakMap<object, string>();

function newRuleUid(): string {
  counter += 1;
  return `r${counter}`;
}

/** A copy of `rule` with its own uid (a new one unless `uid` is given). */
export function withRuleUid<T extends object>(rule: T, uid: string = newRuleUid()): T {
  return { ...rule, [UID]: uid };
}

/** The uid of a rule. */
export function ruleUid(rule: object): string {
  const own = (rule as Tagged)[UID];
  if (own) return own;
  let uid = untagged.get(rule);
  if (!uid) {
    uid = newRuleUid();
    untagged.set(rule, uid);
  }
  return uid;
}

/**
 * Re-read rules keep their uids: each rule in `next` takes the uid of the rule
 * with the same name in `previous` (the editor's last server snapshot), else a
 * new one. After an apply the editor re-reads the section, and the rules it
 * applied must keep the uids that the editor value still carries.
 */
export function carryRuleUids<T extends { name: string }>(next: readonly T[], previous: readonly T[]): T[] {
  const byName = new Map<string, string>();
  for (const r of previous) if (r.name && !byName.has(r.name)) byName.set(r.name, ruleUid(r));
  const used = new Set<string>();
  return next.map((r) => {
    const uid = byName.get(r.name);
    if (uid && !used.has(uid)) {
      used.add(uid);
      return withRuleUid(r, uid);
    }
    return withRuleUid(r);
  });
}
