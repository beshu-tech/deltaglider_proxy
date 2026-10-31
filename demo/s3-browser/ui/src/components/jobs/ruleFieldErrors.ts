/**
 * THE field checks of a replication or a lifecycle rule (UX harvest D9, D10).
 *
 * One pure function per rule kind gives the error sentence of each field,
 * from the `v.*` validators of the validation framework. The rule editor
 * (ReplicationRuleFields / LifecycleRuleFields) shows each one under its
 * field; the rules page counts them for the dirty bar and refuses Review &
 * apply while one is left. Nothing is a toast.
 *
 * A number field stores what the user typed: a whole number as typed (out
 * of range included), `NaN` for text that is not a whole number (see
 * `RuleNumberInput`). So the check reads the stored value, and a typo is
 * never clamped into a valid-looking number.
 */
import type { LifecycleRuleConfig, ReplicationRuleConfig } from '../../adminApi';
import { v } from '../../validation';
import { lifecycleRuleFieldErrors, replicationRuleFieldErrors } from './humanDuration';

export type RuleFieldErrors = Partial<Record<string, string>>;

/** The text of a stored number for a validator: '' for none, 'NaN' for a typo. */
const numText = (n: number | null | undefined) => (n === undefined || n === null ? '' : String(n));

const BATCH = v.integer({ min: 1, max: 10_000 });

function nameError(rule: { name: string }, rules: readonly { name: string }[]): string | null {
  const error = v.ruleName()(rule.name);
  if (error) return error;
  const same = rules.filter((r) => r.name === rule.name).length;
  return same > 1 ? 'Another rule has this name. Use a name that is unique on this page.' : null;
}

function put(out: RuleFieldErrors, field: string, error: string | null) {
  if (error) out[field] = error;
}

/** Pure: the errors of one replication rule, by field. */
export function replicationRuleErrors(
  rule: ReplicationRuleConfig,
  rules: readonly ReplicationRuleConfig[] = [rule],
): RuleFieldErrors {
  const out: RuleFieldErrors = {};
  put(out, 'name', nameError(rule, rules));
  put(out, 'source', rule.source.bucket.trim() ? null : 'Choose the bucket to copy from.');
  put(out, 'destination', rule.destination.bucket.trim() ? null : 'Choose the bucket to copy into.');
  Object.assign(out, replicationRuleFieldErrors(rule));
  put(out, 'batch_size', BATCH(numText(rule.batch_size)));
  return out;
}

/** Pure: the errors of one lifecycle rule, by field. */
export function lifecycleRuleErrors(
  rule: LifecycleRuleConfig,
  rules: readonly LifecycleRuleConfig[] = [rule],
): RuleFieldErrors {
  const out: RuleFieldErrors = {};
  put(out, 'name', nameError(rule, rules));
  put(out, 'bucket', rule.bucket.trim() ? null : 'Choose the bucket that this rule cleans up.');
  Object.assign(out, lifecycleRuleFieldErrors(rule));
  const action = typeof rule.action === 'object' ? rule.action : null;
  if (action?.type === 'retain-newest') {
    put(out, 'count', v.integer({ min: 1, max: 100_000 })(numText(action.count)));
    put(out, 'min_size_bytes', v.optional(v.integer({ min: 0 }))(numText(action.qualify?.min_size_bytes)));
  }
  if ((action?.type === 'transition' || action?.type === 'archive') && !action.destination?.bucket?.trim()) {
    out.destination = 'Choose the bucket to move the objects into.';
  }
  put(out, 'batch_size', BATCH(numText(rule.batch_size)));
  return out;
}

/** Pure: the number a person typed into a whole-number field. Empty →
 *  undefined; digits (with an optional minus) → that number, also out of
 *  range; anything else ("abc", "1e3", "12.5") → NaN. Never clamped: the
 *  field's validator says what is wrong (UX harvest D9). */
export function parseTypedInteger(text: string): number | undefined {
  const t = text.trim();
  if (t === '') return undefined;
  return /^-?\d+$/.test(t) ? Number(t) : Number.NaN;
}
