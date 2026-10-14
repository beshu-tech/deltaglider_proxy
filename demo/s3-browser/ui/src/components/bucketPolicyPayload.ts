/**
 * Pure payload + validation logic for BucketsPanel.
 *
 * Extracted to a React-free module (no antd / no hooks) so the Node
 * unit test can import it directly, and so the
 * fetch→dirty→validate→PUT pipeline can move onto the shared
 * `useSectionEditor` hook without duplicating the storage-section
 * apply machinery.
 *
 * The local editing shape carries stable synthetic ids (`_id` on each
 * row, `{ id, value }` on each public prefix) so React keys by identity,
 * not array index. Those ids are NEVER serialised — `rowToPolicy`
 * strips them on the way to the wire.
 *
 * ## Merge-patch semantics (the part that bit us)
 *
 * The storage-section PUT applies RFC 7396 JSON Merge Patch RECURSIVELY:
 * per-bucket entries deep-merge, `null` deletes a key, and an ABSENT key
 * preserves the server's old value. The original builder only emitted an
 * explicit `null` for `compression` — so every other "unset" (clearing a
 * route/alias/quota/ratio, switching a public bucket back to private,
 * deleting a whole policy) silently no-opped server-side: the UI said
 * "applied", the YAML kept the old value, and the row resurrected on the
 * next load. Un-publicking a bucket not taking effect is the security-
 * relevant case.
 *
 * The contract now: `rowToPolicy` emits EVERY clearable field explicitly
 * (value or `null`), and `buildBucketPayload` takes the BASELINE bucket
 * names (what the server had at fetch time) so removed/reset policies
 * serialise as `name: null` — the merge-patch spelling of "delete this
 * policy".
 */
import type { AdminConfig } from '../adminApi';

/** A public-prefix entry carrying a stable synthetic id so the
 *  prefix list keys by identity, not array index. */
export interface PrefixEntry {
  id: string;
  value: string;
}

/** Local working shape — mirrors the backend `BucketPolicyConfig`
 *  but normalises nulls to undefined for the form controllers. */
export interface BucketPolicyRow {
  /** Stable synthetic id — React key + mutate-by-id target. Never serialised. */
  _id: string;
  name: string;
  /** `null` = omit key / YAML null — inherit engine default (delta enabled). */
  compression: boolean | null;
  max_delta_ratio: number | null;
  backend: string;
  alias: string;
  /** Tri-state source of truth for the anonymous-read radio group. */
  publicMode: 'none' | 'entire' | 'prefixes';
  /** Specific prefixes — only surfaced when `publicMode === 'prefixes'`.
   *  Local editing shape carries stable ids; converted to/from the wire
   *  `string[]` in policyToRow / rowToPolicy. */
  public_prefixes: PrefixEntry[];
  quota_bytes: number | null;
  /** Read-only passthrough of the server marker (no editor in this panel —
   *  set it in YAML). Carried through the row model so a bucket whose ONLY
   *  policy is the marker doesn't round-trip as "all default" and get its
   *  policy DELETED on apply (the silent-field-loss bug class). */
  replication_target_only: boolean;
  /** The bucket's own `encryption` block as the server returns it (keys
   *  redacted), issue #116. Read-only here: the card's Encryption group
   *  changes it with its own immediate apply (BucketEncryptionGroup), so it
   *  is never serialised by this panel; the merge-patch keeps an absent key.
   *  A row with a block is never all-default (a `null` policy would remove
   *  the bucket's key, which the server refuses), and "Reset to defaults"
   *  keeps it and the backend route that it needs. Optional and outside
   *  `DEFAULT_ROW_FIELDS`. */
  encryption?: Readonly<Record<string, unknown>> | null;
  /** The server policy's fields that this panel does NOT model (any field
   *  a later server adds), carried verbatim. Never serialised: the merge-patch keeps an absent
   *  key, so the server keeps them. A row with such a field is never
   *  all-default, so it is never sent as `null` (which would delete the
   *  policy, its key with it). Optional and outside `DEFAULT_ROW_FIELDS`,
   *  so "reset to defaults" keeps it. */
  unmodeled?: Readonly<Record<string, unknown>>;
}

/** The policy fields this panel models (everything else is `unmodeled`). */
const MODELED_KEYS: ReadonlySet<string> = new Set([
  'compression',
  'max_delta_ratio',
  'backend',
  'alias',
  'public',
  'public_prefixes',
  'quota_bytes',
  'replication_target_only',
  'encryption',
]);

/** The fields of a server policy that this panel does not model. */
function unmodeledFields(p: object): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(p)) {
    if (!MODELED_KEYS.has(k) && v !== undefined && v !== null) out[k] = v;
  }
  return out;
}

function hasUnmodeled(row: BucketPolicyRow): boolean {
  return Object.keys(row.unmodeled ?? {}).length > 0;
}

/** The row holds the bucket's own encryption block. */
export function hasOwnEncryption(row: Pick<BucketPolicyRow, 'encryption'> | null | undefined): boolean {
  return row?.encryption != null;
}

/** The mode of the row's own encryption block, if it has one. */
export function ownEncryptionMode(row: Pick<BucketPolicyRow, 'encryption'>): string | null {
  const mode = (row.encryption as { mode?: unknown } | null | undefined)?.mode;
  return typeof mode === 'string' ? mode : null;
}

/** Whether a server policy must survive every apply of this panel: it has
 *  the bucket's own encryption (its keys) or a field this panel does not
 *  model. Such a policy is never sent as `null`. */
export function policyMustSurvive(p: object): boolean {
  return Object.keys(unmodeledFields(p)).length > 0 || (p as { encryption?: unknown }).encryption != null;
}

/** "Reset to defaults" of a row: every field this panel edits goes back to
 *  its default, except the bucket's own encryption and the backend route
 *  that the block needs (the key must stay readable). */
export function resetRowFields(row: BucketPolicyRow | null): Partial<BucketPolicyRow> {
  const out: Partial<BucketPolicyRow> = { ...DEFAULT_ROW_FIELDS, public_prefixes: [] };
  if (row && hasOwnEncryption(row)) out.backend = row.backend;
  return out;
}

/** One bucket's merge-patch: every clearable field present (value or null). */
export interface BucketPolicyPatch {
  compression: boolean | null;
  max_delta_ratio: number | null;
  backend: string | null;
  alias: string | null;
  /** Always emitted as null — the [""]-sentinel in `public_prefixes` is the
   *  single wire spelling for "entire bucket"; nulling `public` clears a
   *  stored `public: true` shorthand when leaving that mode. */
  public: null;
  public_prefixes: string[] | null;
  quota_bytes: number | null;
  /** `true` preserved verbatim; `null` clears (the field has no editor here,
   *  so it always mirrors what the server sent at fetch time). */
  replication_target_only: boolean | null;
}

/** The storage-section merge body: per-bucket patch, or `null` = delete policy. */
type BucketsPatchBody = { buckets: Record<string, BucketPolicyPatch | null> };

/** Default (no-override) field values — what a bucket without a policy means. */
export const DEFAULT_ROW_FIELDS: Omit<BucketPolicyRow, '_id' | 'name' | 'unmodeled'> = Object.freeze({
  compression: null,
  max_delta_ratio: null,
  backend: '',
  alias: '',
  publicMode: 'none' as const,
  public_prefixes: [],
  quota_bytes: null,
  replication_target_only: false,
});

let rowIdCounter = 0;

/** Monotonic, collision-free row id (stable React key; never reused). */
export const freshId = (): string => `bkt-${++rowIdCounter}`;

export function policyToRow(
  name: string,
  p: NonNullable<AdminConfig['bucket_policies']>[string]
): BucketPolicyRow {
  // Determine the tri-state from the persisted shape:
  //   * `public: true` (shorthand)          -> entire
  //   * `public_prefixes: [""]` (expanded)   -> entire
  //   * `public_prefixes: ["builds/", ...]`  -> prefixes
  //   * anything else                        -> none
  let publicMode: 'none' | 'entire' | 'prefixes' = 'none';
  let prefixes: string[] = [];
  if (p.public === true) {
    publicMode = 'entire';
  } else if (p.public_prefixes && p.public_prefixes.length > 0) {
    if (p.public_prefixes.length === 1 && p.public_prefixes[0] === '') {
      publicMode = 'entire';
    } else {
      publicMode = 'prefixes';
      prefixes = p.public_prefixes.slice();
    }
  }
  return {
    _id: freshId(),
    name,
    compression:
      p.compression === undefined || p.compression === null ? null : p.compression,
    max_delta_ratio: p.max_delta_ratio ?? null,
    backend: p.backend ?? '',
    alias: p.alias ?? '',
    publicMode,
    public_prefixes: prefixes.map((value) => ({ id: freshId(), value })),
    quota_bytes: p.quota_bytes ?? null,
    replication_target_only: p.replication_target_only ?? false,
    encryption: (p as { encryption?: Record<string, unknown> | null }).encryption ?? null,
    unmodeled: unmodeledFields(p),
  };
}

/** The row's non-blank public prefixes (what would actually serialise). */
function cleanedPrefixes(row: BucketPolicyRow): string[] {
  return row.public_prefixes.map((p) => p.value.trim()).filter((p) => p.length > 0);
}

/**
 * True when the row overrides NOTHING — i.e. the bucket behaves exactly as
 * if it had no policy. Such rows serialise as policy deletion (when the
 * server has a policy) or nothing at all (when it doesn't): a policy exists
 * iff something is overridden.
 */
export function isAllDefaultRow(row: BucketPolicyRow): boolean {
  return (
    !hasUnmodeled(row) &&
    !hasOwnEncryption(row) &&
    row.compression === null &&
    row.max_delta_ratio === null &&
    row.backend === '' &&
    row.alias === '' &&
    row.quota_bytes === null &&
    !row.replication_target_only &&
    (row.publicMode === 'none' ||
      (row.publicMode === 'prefixes' && cleanedPrefixes(row).length === 0))
  );
}

function rowToPolicy(row: BucketPolicyRow): BucketPolicyPatch {
  // Serialise the tri-state back to the wire shape the backend accepts.
  // `entire` uses the empty-string sentinel `[""]` — the backend's
  // `BucketPolicyConfig::normalize` collapses it to `public: true` on
  // re-serialisation, lossless round-trip.
  //
  // EVERY clearable field is emitted explicitly: a concrete value, or
  // `null` so the RFC 7396 merge deletes the old key. Omission would
  // PRESERVE the server's previous value — the silent-no-op bug class
  // described in the module header.
  let public_prefixes: string[] | null = null;
  if (row.publicMode === 'entire') {
    public_prefixes = [''];
  } else if (row.publicMode === 'prefixes') {
    const cleaned = cleanedPrefixes(row);
    if (cleaned.length > 0) public_prefixes = cleaned;
  }
  return {
    compression: row.compression === null ? null : row.compression,
    max_delta_ratio: row.max_delta_ratio ?? null,
    backend: row.backend || null,
    alias: row.alias || null,
    public: null,
    public_prefixes,
    quota_bytes: row.quota_bytes ?? null,
    replication_target_only: row.replication_target_only ? true : null,
  };
}

type BucketPayloadResult =
  | { ok: true; body: BucketsPatchBody }
  | { ok: false; error: string };

/**
 * Validate the rows + build the `{ buckets }` storage-section merge body.
 *
 * Validation: bucket names must be non-empty (empty rows are
 * genuinely-unfilled and dropped); duplicate names abort with an error.
 *
 * `baselineNames` is the set of bucket names that HAD a policy on the
 * server at fetch time. Any baseline bucket that is now absent from the
 * rows — or present but all-default — serialises as `name: null`, the
 * merge-patch deletion. Without it, removing a policy is a server-side
 * no-op (see module header).
 */
export function buildBucketPayload(
  rows: BucketPolicyRow[],
  baselineNames: readonly string[] = [],
  /** Baseline buckets whose server policy must survive (`policyMustSurvive`:
   *  the bucket's own encryption, or fields this panel does not model): a
   *  removed row of one of them clears the edited fields only, never
   *  `null` (issue #116 review C). */
  unmodeledNames: readonly string[] = []
): BucketPayloadResult {
  const cleaned = rows.filter((r) => r.name.trim());
  const names = cleaned.map((r) => r.name);
  const dupes = names.filter((n, i) => names.indexOf(n) !== i);
  if (dupes.length > 0) {
    return { ok: false, error: `Duplicate bucket name: ${dupes[0]}` };
  }
  const bp: Record<string, BucketPolicyPatch | null> = {};
  for (const row of cleaned) {
    if (isAllDefaultRow(row)) {
      // No overrides → no policy. Delete the server's policy if one exists;
      // otherwise emit nothing (a brand-new all-default row is a no-op).
      if (baselineNames.includes(row.name)) bp[row.name] = null;
    } else {
      bp[row.name] = rowToPolicy(row);
    }
  }
  for (const name of baselineNames) {
    if (cleaned.some((r) => r.name === name)) continue;
    bp[name] = unmodeledNames.includes(name)
      ? rowToPolicy({ _id: '', name, ...DEFAULT_ROW_FIELDS, public_prefixes: [] })
      : null;
  }
  return { ok: true, body: { buckets: bp } };
}
