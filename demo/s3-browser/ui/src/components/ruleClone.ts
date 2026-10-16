/**
 * Pure helpers for cloning a replication or lifecycle rule, and for spotting
 * two rules that act on the same objects (the usual mistake after a clone).
 */
import type { ReplicationRuleConfig } from "../adminApi/replication";
import type { LifecycleRuleConfig } from "../adminApi/lifecycle";
import { withRuleUid } from "../ruleUid";
import { actionKind } from "./lifecyclePayload";

/** `<name>-copy`, else `<name>-copy-2`, `-copy-3`, … : the first free name. */
export function copyRuleName(
  name: string,
  existing: ReadonlyArray<{ name: string }>,
): string {
  const base = `${name || "rule"}-copy`;
  const taken = (n: string) => existing.some((r) => r.name === n);
  if (!taken(base)) return base;
  let n = 2;
  while (taken(`${base}-${n}`)) n += 1;
  return `${base}-${n}`;
}

/** A deep copy of `rule` with a free name and its own uid (see ruleUid.ts). */
export function cloneRule<T extends { name: string }>(
  rule: T,
  existing: ReadonlyArray<{ name: string }>,
): T {
  const copy = JSON.parse(JSON.stringify(rule)) as T;
  return withRuleUid({ ...copy, name: copyRuleName(rule.name, existing) });
}

const globs = (g: readonly string[]) => [...g].sort().join("\n");

function replicationScope(r: ReplicationRuleConfig): string | null {
  if (!r.source.bucket || !r.destination.bucket) return null;
  return JSON.stringify([
    r.source.bucket,
    r.source.prefix,
    r.destination.bucket,
    r.destination.prefix,
    globs(r.include_globs),
    globs(r.exclude_globs),
  ]);
}

function lifecycleScope(r: LifecycleRuleConfig): string | null {
  if (!r.bucket) return null;
  return JSON.stringify([
    r.bucket,
    r.prefix,
    actionKind(r.action),
    globs(r.include_globs),
    globs(r.exclude_globs),
  ]);
}

/**
 * The name of another rule in `rules` that acts on exactly the same objects
 * as `rules[index]`, or null. Replication: the same source, destination and
 * filters. Lifecycle: the same bucket, prefix, action kind and filters.
 */
export function sameScopeRule<K extends "replication" | "lifecycle">(
  kind: K,
  rules: ReadonlyArray<
    K extends "replication" ? ReplicationRuleConfig : LifecycleRuleConfig
  >,
  index: number,
): string | null {
  const scope = (r: ReplicationRuleConfig | LifecycleRuleConfig) =>
    kind === "replication"
      ? replicationScope(r as ReplicationRuleConfig)
      : lifecycleScope(r as LifecycleRuleConfig);
  const own = scope(rules[index]);
  if (own === null) return null;
  const other = rules.find((r, i) => i !== index && scope(r) === own);
  return other ? other.name || "(unnamed)" : null;
}
