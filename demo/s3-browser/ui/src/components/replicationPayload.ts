/**
 * Pure normalize + validation + payload logic for ReplicationPanel.
 *
 * React-free (no antd / no hooks) so the unit test can
 * transpile-and-import it directly, and so the panel can move onto the
 * shared `useSectionEditor` storage-section apply pipeline.
 *
 * `buildReplicationPayload` mirrors the old in-component `buildPayload`:
 * it normalises rules, validates them, and produces the exact
 * `{ replication }` body sent to /validate and PUT — byte-identical to
 * the pre-refactor builder for the same input.
 */
import type {
  ReplicationConfig,
  ReplicationRuleConfig,
  StorageSectionBody,
} from '../adminApi';
import { normalizePrefix } from '../storagePath';
import { nextUniqueRuleName } from './ruleNames';

export const DEFAULT_REPLICATION: ReplicationConfig = {
  enabled: true,
  tick_interval: '30s',
  lease_ttl: '60s',
  heartbeat_interval: '20s',
  max_failures_retained: 100,
  rules: [],
};

export function emptyRule(existing: ReplicationRuleConfig[]): ReplicationRuleConfig {
  return {
    name: nextUniqueRuleName(existing, 'rule'),
    enabled: true,
    source: { bucket: '', prefix: '' },
    destination: { bucket: '', prefix: '' },
    interval: '15m',
    batch_size: 100,
    replicate_deletes: false,
    conflict: 'newer-wins',
    include_globs: [],
    exclude_globs: ['.dg/*'],
  };
}

export function normalizeReplication(
  input: Partial<ReplicationConfig> | undefined
): ReplicationConfig {
  const cfg = { ...DEFAULT_REPLICATION, ...(input || {}) };
  return {
    ...cfg,
    rules: (cfg.rules || []).map((r) => ({
      ...emptyRule([]),
      ...r,
      source: { bucket: r.source?.bucket || '', prefix: r.source?.prefix || '' },
      destination: {
        bucket: r.destination?.bucket || '',
        prefix: r.destination?.prefix || '',
      },
      include_globs: r.include_globs || [],
      exclude_globs: r.exclude_globs || ['.dg/*'],
    })),
  };
}

type ReplicationPayloadResult =
  | { ok: true; body: StorageSectionBody }
  | { ok: false; error: string };

/**
 * Normalise + validate replication rules, then build the
 * `{ replication }` storage-section body. Identical validation order to
 * the pre-refactor in-component `buildPayload`.
 */
export function buildReplicationPayload(
  replication: ReplicationConfig
): ReplicationPayloadResult {
  const normalizedRules = replication.rules.map((rule) => ({
    ...rule,
    source: { ...rule.source, prefix: normalizePrefix(rule.source.prefix) },
    destination: {
      ...rule.destination,
      prefix: normalizePrefix(rule.destination.prefix),
    },
  }));
  const names = normalizedRules.map((r) => r.name.trim()).filter(Boolean);
  const duplicate = names.find((name, idx) => names.indexOf(name) !== idx);
  if (duplicate) {
    return { ok: false, error: `Duplicate rule name: ${duplicate}` };
  }
  for (const rule of normalizedRules) {
    if (!rule.name.trim()) {
      return { ok: false, error: 'Every replication rule needs a name.' };
    }
    if (!rule.source.bucket || !rule.destination.bucket) {
      return {
        ok: false,
        error: `Rule ${rule.name}: source and destination buckets are required.`,
      };
    }
  }
  return { ok: true, body: { replication: { ...replication, rules: normalizedRules } } };
}
