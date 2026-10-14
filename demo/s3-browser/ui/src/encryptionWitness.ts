/**
 * Pure helpers of the encryption witness view (issue #116 P3).
 *
 * On several instances that share a `config_sync_bucket`, each instance
 * compares the encryption of its new writes with the encryption that the
 * cluster published. Where the two differ, that instance refuses writes to
 * the storage. `GET /_/api/admin/encryption-witness` returns the verdict
 * (`null` on a single instance); `POST …/publish` publishes this instance's
 * setting for the named subjects. The rules below map a mismatch to the
 * subject that the server accepts, and a bucket card to its mismatch. They
 * mirror `Config::bucket_storage`, `confirmed_storages` and the gate lookup
 * in the Rust code (src/coordination/encryption_witness.rs).
 */
import type { AdminConfig } from './adminApi';

/** The bucket name that stands for a backend's buckets with no entry of their own. */
const ALL_OTHER_BUCKETS = '*';

/** The `route:default` subject: the `*` storage of the default backend. */
const DEFAULT_ROUTE_SUBJECT = 'route:default';

/** One write encryption, as the witness compares it. */
export interface WitnessIntent {
  mode: string;
  key_id?: string;
  /** Key check value of a proxy key: two keys with one id differ here. */
  kcv?: string;
}

export interface WitnessMismatch {
  /** The raw backend name. */
  backend: string;
  /** The real bucket name, or `*` for the backend's buckets without an entry. */
  bucket: string;
  /** This instance's form. */
  local: WitnessIntent;
  /** The form that the cluster published. */
  published: WitnessIntent;
  /** The instance that published it. */
  set_by: string;
}

export type WitnessState = 'agree' | 'mismatch' | 'unknown' | 'unavailable';

export interface WitnessStatus {
  status: WitnessState;
  mismatches?: WitnessMismatch[];
  error?: string;
}

/** The part of the config that decides where a bucket is stored. */
export type WitnessConfig = Pick<AdminConfig, 'bucket_policies' | 'backends' | 'default_backend'>;

/** The default backend's name, as `Config::default_backend_name` resolves it. */
function defaultBackendName(cfg: WitnessConfig): string {
  if (cfg.backends.length === 0) return 'default';
  return cfg.default_backend ?? cfg.backends[0].name;
}

/** `Config::bucket_storage`: the (backend, real bucket) that the config fixes for `name`, or null. */
function bucketStorage(cfg: WitnessConfig, name: string): [string, string] | null {
  const lower = name.toLowerCase();
  if (cfg.backends.length === 0) return ['default', lower];
  const policy = cfg.bucket_policies?.[lower];
  if (!policy?.backend) return null;
  return [policy.backend, policy.alias || lower];
}

/** Plaintext: the only form that a publish must confirm as a downgrade. */
export function isPlaintext(intent: WitnessIntent): boolean {
  return intent.mode === 'none';
}

/** One line for an intent: "aes256-gcm-proxy · key k1 (check a1b2c3d4…)". */
export function describeIntent(intent: WitnessIntent): string {
  if (isPlaintext(intent)) return 'none (plaintext)';
  const key = intent.key_id
    ? ` · key ${intent.key_id}${intent.kcv ? ` (check ${intent.kcv})` : ''}`
    : intent.mode === 'aes256-gcm-proxy'
      ? ' · no usable key'
      : '';
  return `${intent.mode}${key}`;
}

/** What the mismatch covers, in words. */
export function storageLabel(m: WitnessMismatch): string {
  return m.bucket === ALL_OTHER_BUCKETS ? 'All buckets without their own setting' : m.bucket;
}

/**
 * The publish subject that names the mismatch's storage:
 *   * a bucket that the config places on this storage → its name;
 *   * `*` of the default backend → `route:default`;
 *   * anything else → `backend:<name>`, which publishes EVERY storage of
 *     this instance on that backend (the server has no narrower subject for
 *     a non-default backend's `*` or for a bucket without a route).
 */
export function publishSubject(m: WitnessMismatch, cfg: WitnessConfig): string {
  if (m.bucket === ALL_OTHER_BUCKETS) {
    return m.backend === defaultBackendName(cfg) ? DEFAULT_ROUTE_SUBJECT : `backend:${m.backend}`;
  }
  const names = cfg.backends.length === 0 ? [m.bucket] : Object.keys(cfg.bucket_policies ?? {});
  const own = names.find((n) => {
    const s = bucketStorage(cfg, n);
    return s !== null && s[0] === m.backend && s[1] === m.bucket;
  });
  return own ?? `backend:${m.backend}`;
}

/** The body and the downgrade confirmations of one publish of `mismatches`. */
export function publishPlan(
  mismatches: WitnessMismatch[],
  cfg: WitnessConfig,
): { storages: string[]; downgrades: string[] } {
  const storages: string[] = [];
  const downgrades: string[] = [];
  for (const m of mismatches) {
    const subject = publishSubject(m, cfg);
    if (!storages.includes(subject)) storages.push(subject);
    if (isPlaintext(m.local) && !downgrades.includes(subject)) downgrades.push(subject);
  }
  return { storages, downgrades };
}

/**
 * The mismatch that refuses writes to bucket `name` on this instance, or
 * null. `backend` is the raw backend that stores the bucket now. The gate
 * looks up the bucket's own entry first and the backend's `*` entry
 * otherwise. A bucket that the config names and places is judged on its own
 * entry (the witness lists it by name when it differs); any other bucket
 * follows its backend's `*`. (Such a bucket can still have an own entry from
 * an earlier config; the UI does not see that and shows the `*` verdict.)
 */
export function bucketMismatch(
  mismatches: WitnessMismatch[],
  cfg: WitnessConfig,
  name: string,
  backend: string | null | undefined,
): WitnessMismatch | null {
  const lower = name.toLowerCase();
  const placed = cfg.bucket_policies?.[lower] !== undefined ? bucketStorage(cfg, lower) : null;
  if (placed) {
    return mismatches.find((m) => m.backend === placed[0] && m.bucket === placed[1]) ?? null;
  }
  if (!backend) return null;
  return (
    mismatches.find((m) => m.backend === backend && m.bucket === lower) ??
    mismatches.find((m) => m.backend === backend && m.bucket === ALL_OTHER_BUCKETS) ??
    null
  );
}
