/**
 * Fields whose value a `DGP_*` environment variable controls. The server
 * reports them in `GET /api/admin/config` → `env_overrides` (see
 * `src/config/env_overrides.rs`). Every control for such a field is
 * read-only: FormField through `yamlPath`, other controls through the
 * `useEnvOverride` hook. Pure, unit-tested.
 */
export interface EnvOverride {
  env: string;
  /** Absent for settings that exist only as environment variables. */
  yaml_path?: string;
  secret: boolean;
  /** Absent for secrets (the server never sends their value) and unset block members. */
  value?: string;
  /**
   * False when `env` itself is unset: the field is env-controlled only
   * because `activated_by` replaced its whole block (for example
   * `DGP_S3_ENDPOINT` controls all of `storage.backend`).
   */
  set?: boolean;
  activated_by?: string;
}

/** The override for a field, matched by YAML path or (env-only) by name. */
export function findEnvOverride(
  overrides: readonly EnvOverride[] | undefined,
  yamlPath?: string,
  envVar?: string
): EnvOverride | null {
  if (!overrides) return null;
  return (
    overrides.find(
      (o) => (yamlPath !== undefined && o.yaml_path === yamlPath) || (envVar !== undefined && o.env === envVar)
    ) ?? null
  );
}

/** True when the variable itself is set (older servers omit `set`). */
function isSet(o: EnvOverride): boolean {
  return o.set !== false;
}

/** Text shown in place of the input for an overridden field. */
export function envOverrideText(o: EnvOverride): string {
  if (!isSet(o)) return o.value !== undefined ? `${o.value} (default)` : 'Not set';
  if (o.secret || o.value === undefined) return 'Set from the environment (value hidden)';
  return o.value === '' ? '(empty)' : o.value;
}

/** The variable that controls the field, for the badge. */
export function envOverrideSource(o: EnvOverride): string {
  return isSet(o) ? o.env : (o.activated_by ?? o.env);
}

/** One-sentence explanation under an overridden field. */
export function envOverrideHelp(o: EnvOverride): string {
  if (isSet(o)) {
    return `The ${o.env} environment variable sets this value. Change it there and restart the proxy.`;
  }
  const by = o.activated_by ?? o.env;
  return `The ${by} environment variable controls this whole group of settings. To change this value, set ${o.env} and restart the proxy.`;
}

/**
 * YAML path of a backend's encryption field in `env_overrides`. The
 * singleton backend (no named list) lives under `storage.backend_encryption`.
 * Mirrors `backend_encryption_path` in `src/config/env_overrides.rs`.
 */
export function backendEncryptionEnvPath(backendName: string | null, field: 'key' | 'kms_key_id'): string {
  return backendName === null
    ? `storage.backend_encryption.${field}`
    : `storage.backends[${backendName}].encryption.${field}`;
}
