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
 *
 * Invalidation matches by key PREFIX, so a `list()` key must never be a
 * prefix of a sibling: `['users']` would also refetch canned policies on
 * every user edit. Lists get their own `'list'` segment; `all()` is the
 * explicit root when a broad refresh is intended.
 */
export const qk = {
  docs: () => ['docs'] as const,

  // ── Config ──────────────────────────────────────────────────────
  config: () => ['config'] as const,

  // ── IAM ─────────────────────────────────────────────────────────
  users: {
    list: () => ['users', 'list'] as const,
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
  externalIdentities: {
    list: () => ['external-identities'] as const,
  },

  // ── Storage ─────────────────────────────────────────────────────
  backends: {
    // Deliberately a prefix of `origins`: a backend create/delete moves
    // buckets, so BackendsPanel's refresh() of the list also refetches origins.
    list: () => ['backends'] as const,
    origins: () => ['backends', 'origins'] as const,
    // NOT under the `backends` prefix: every scan HEADs every object, so a
    // list invalidation must not start one.
    legacyKeyUsage: (name: string) => ['legacy-key-usage', name] as const,
  },

  // ── Diagnostics ─────────────────────────────────────────────────
  bucketUsage: (bucket: string) => ['bucket-usage', bucket] as const,

  // ── Jobs (replication / lifecycle / reencrypt / migrate) ────────
  jobs: {
    all: () => ['jobs'] as const,
    list: () => ['jobs', 'list'] as const,
    runs: (id: string) => ['jobs', 'runs', id] as const,
    failures: (id: string) => ['jobs', 'failures', id] as const,
    preview: (id: string) => ['jobs', 'preview', id] as const,
    verify: (rule: string) => ['jobs', 'verify', rule] as const,
  },
  // Per-bucket busy banner (session-light endpoint).
  maintenance: {
    bucket: (bucket: string) => ['maintenance', 'bucket', bucket] as const,
  },
} as const;
