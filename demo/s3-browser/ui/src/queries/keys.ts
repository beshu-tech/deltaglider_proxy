/**
 * Centralised query-key factory.
 *
 * Single source of truth for every TanStack Query cache key in the app.
 * Hand-rolled `[ 'users' ]` strings sprinkled across components are how
 * cache-invalidation goes wrong: a mutation invalidates `['user']` while
 * a query reads `['users']` — the typo is silent and the UI lies.
 *
 * Convention: every key is a tuple starting with the resource family,
 * narrowed by parameters. `qk.users.list()` reads as a path so callers
 * naturally think in resources.
 */
export const qk = {
  // ── Auth / session ──────────────────────────────────────────────
  whoami: () => ['whoami'] as const,
  session: () => ['session'] as const,

  // ── Config ──────────────────────────────────────────────────────
  config: () => ['config'] as const,
  configSection: (name: string) => ['config', 'section', name] as const,
  configYaml: () => ['config', 'yaml'] as const,
  configDefaults: () => ['config', 'defaults'] as const,

  // ── IAM ─────────────────────────────────────────────────────────
  users: {
    list: () => ['users'] as const,
    cannedPolicies: () => ['users', 'canned-policies'] as const,
  },
  groups: {
    list: () => ['groups'] as const,
  },
  authProviders: {
    list: () => ['auth-providers'] as const,
  },
  groupMappingRules: {
    list: () => ['group-mapping-rules'] as const,
  },

  // ── Storage ─────────────────────────────────────────────────────
  backends: {
    list: () => ['backends'] as const,
  },

  // ── Diagnostics ─────────────────────────────────────────────────
  audit: (limit?: number) => ['audit', { limit }] as const,
  metrics: () => ['metrics'] as const,
  stats: () => ['stats'] as const,
  health: () => ['health'] as const,
  prefixUsage: (bucket: string, prefix: string) =>
    ['prefix-usage', bucket, prefix] as const,

  // ── Replication ─────────────────────────────────────────────────
  replication: {
    overview: () => ['replication'] as const,
    history: (rule: string, limit?: number) =>
      ['replication', 'history', rule, { limit }] as const,
    failures: (rule: string, limit?: number) =>
      ['replication', 'failures', rule, { limit }] as const,
  },
} as const;
