/**
 * Pure normalize + validation + payload logic for LifecyclePanel.
 *
 * React-free (no antd / no hooks) so the Node regression script can
 * transpile-and-import it directly, and so the panel can move onto the
 * shared `useSectionEditor` storage-section apply pipeline.
 *
 * `buildLifecyclePayload` mirrors the old in-component `buildPayload`:
 * it normalises rules, validates them, and produces the exact
 * `{ lifecycle }` body sent to /validate and PUT — byte-identical to
 * the pre-refactor builder for the same input.
 */
import type {
  LifecycleAction,
  LifecycleConfig,
  LifecycleRuleConfig,
  StorageSectionBody,
} from '../adminApi';
import { normalizePrefix } from '../storagePath';
import { nextUniqueRuleName } from './ruleNames';

export const DEFAULT_LIFECYCLE: LifecycleConfig = {
  enabled: false,
  tick_interval: '1h',
  max_failures_retained: 100,
  rules: [],
};

export function emptyRule(existing: LifecycleRuleConfig[]): LifecycleRuleConfig {
  return {
    name: nextUniqueRuleName(existing, 'expire-old'),
    enabled: false,
    bucket: '',
    prefix: '',
    action: 'delete',
    expire_after: '30d',
    include_globs: [],
    exclude_globs: ['.deltaglider/**'],
    batch_size: 100,
  };
}

export function actionKind(
  action: LifecycleRuleConfig['action']
): 'delete' | 'transition' | 'retain-newest' {
  if (typeof action === 'object' && action?.type) {
    return action.type === 'retain-newest' ? 'retain-newest' : 'transition';
  }
  return 'delete';
}

function normalizeAction(
  action: LifecycleRuleConfig['action']
): LifecycleRuleConfig['action'] {
  const kind = actionKind(action);
  if (kind === 'delete' || typeof action !== 'object') return 'delete';
  if (kind === 'retain-newest') {
    const a = action as Extract<LifecycleAction, { type: 'retain-newest' }>;
    // Drop empty qualify fields so the YAML stays minimal and round-trips.
    const qualify: { min_size_bytes?: number; min_age?: string } = {};
    if (typeof a.qualify?.min_size_bytes === 'number' && a.qualify.min_size_bytes > 0) {
      qualify.min_size_bytes = a.qualify.min_size_bytes;
    }
    if (a.qualify?.min_age?.trim()) qualify.min_age = a.qualify.min_age.trim();
    // Do NOT clamp count here — an invalid (< 1) count must reach the validator
    // so the operator sees their mistake, rather than being silently "fixed".
    // (The InputNumber min=1 already prevents it via the UI; this guards the
    // import/YAML-paste path.) We floor non-integers but preserve the magnitude.
    const out: Extract<LifecycleAction, { type: 'retain-newest' }> = {
      type: 'retain-newest',
      count: Math.floor(Number(a.count)),
    };
    if (Object.keys(qualify).length > 0) out.qualify = qualify;
    if (a.protect_younger_than?.trim()) out.protect_younger_than = a.protect_younger_than.trim();
    return out;
  }
  const t = action as Extract<LifecycleAction, { type: 'transition' | 'archive' }>;
  return {
    type: 'transition',
    destination: {
      bucket: t.destination?.bucket?.trim() || '',
      prefix: normalizePrefix(t.destination?.prefix || ''),
    },
    delete_source_after_success: Boolean(t.delete_source_after_success),
  };
}

export function actionLabel(
  action: LifecycleRuleConfig['action'] | string | undefined
): string {
  switch (actionKind(action as LifecycleRuleConfig['action'])) {
    case 'transition':
      return 'archive/move';
    case 'retain-newest':
      return 'retain newest';
    default:
      return 'delete';
  }
}

export function normalizeLifecycle(
  input: Partial<LifecycleConfig> | undefined
): LifecycleConfig {
  const cfg = { ...DEFAULT_LIFECYCLE, ...(input || {}) };
  return {
    ...cfg,
    rules: (cfg.rules || []).map((rule) => ({
      ...emptyRule([]),
      ...rule,
      action: normalizeAction(rule.action),
      prefix: rule.prefix || '',
      include_globs: rule.include_globs || [],
      exclude_globs: rule.exclude_globs || ['.deltaglider/**'],
      batch_size: rule.batch_size || 100,
    })),
  };
}

type LifecyclePayloadResult =
  | { ok: true; body: StorageSectionBody }
  | { ok: false; error: string };

/**
 * Normalise + validate lifecycle rules, then build the `{ lifecycle }`
 * storage-section body. Identical validation order to the pre-refactor
 * in-component `buildPayload`.
 */
export function buildLifecyclePayload(
  lifecycle: LifecycleConfig
): LifecyclePayloadResult {
  const normalizedRules = lifecycle.rules.map((rule) => {
    const action = normalizeAction(rule.action);
    // retain-newest selects by count and has no expire_after; drop any stray
    // value so it never reaches the wire (the backend warns on a stray one).
    const expire_after =
      actionKind(action) === 'retain-newest'
        ? undefined
        : (rule.expire_after || '').trim();
    return {
      ...rule,
      action,
      name: rule.name.trim(),
      bucket: rule.bucket.trim(),
      prefix: normalizePrefix(rule.prefix),
      expire_after,
      batch_size: rule.batch_size || 100,
    };
  });
  const names = normalizedRules.map((r) => r.name).filter(Boolean);
  const duplicate = names.find((name, idx) => names.indexOf(name) !== idx);
  if (duplicate) {
    return { ok: false, error: `Duplicate rule name: ${duplicate}` };
  }
  for (const rule of normalizedRules) {
    if (!rule.name) {
      return { ok: false, error: 'Every lifecycle rule needs a name.' };
    }
    if (!/^[A-Za-z0-9_.-]{1,64}$/.test(rule.name)) {
      return {
        ok: false,
        error: `Rule ${rule.name}: names must match [A-Za-z0-9_.-]{1,64}.`,
      };
    }
    if (!rule.bucket) {
      return { ok: false, error: `Rule ${rule.name}: bucket is required.` };
    }
    const kind = actionKind(rule.action);
    if (kind === 'retain-newest') {
      const action = rule.action as Extract<LifecycleAction, { type: 'retain-newest' }>;
      if (!Number.isFinite(action.count) || action.count < 1) {
        return {
          ok: false,
          error: `Rule ${rule.name}: retain-newest count must be at least 1.`,
        };
      }
    } else {
      if (!rule.expire_after) {
        return { ok: false, error: `Rule ${rule.name}: expire_after is required.` };
      }
      if (kind === 'transition') {
        const action = rule.action as Extract<
          LifecycleAction,
          { type: 'transition' | 'archive' }
        >;
        if (!action.destination.bucket.trim()) {
          return {
            ok: false,
            error: `Rule ${rule.name}: transition destination bucket is required.`,
          };
        }
      }
    }
  }
  return {
    ok: true,
    body: {
      lifecycle: {
        ...lifecycle,
        rules: normalizedRules,
      },
    },
  };
}
