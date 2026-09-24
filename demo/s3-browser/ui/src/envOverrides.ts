/**
 * Fields whose value a `DGP_*` environment variable controls. The server
 * reports them in `GET /api/admin/config` → `env_overrides` (see
 * `src/config/env_overrides.rs`); FormField looks its field up here and shows
 * the effective value read-only with a "from env" badge. Pure, unit-tested.
 */
export interface EnvOverride {
  env: string;
  /** Absent for settings that exist only as environment variables. */
  yaml_path?: string;
  secret: boolean;
  /** Absent for secrets: the server never sends their value. */
  value?: string;
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

/** Text shown in place of the input for an overridden field. */
export function envOverrideText(o: EnvOverride): string {
  if (o.secret || o.value === undefined) return 'Set from the environment (value hidden)';
  return o.value === '' ? '(empty)' : o.value;
}
