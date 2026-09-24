/**
 * Pure helpers for the setup wizard: is this proxy already configured, and
 * which answers does the current configuration imply? The wizard applies a
 * WHOLE configuration document, so on a configured proxy it must say so
 * before it replaces anything, and it must not preselect a backend type that
 * contradicts the running one.
 */

/** The fields the wizard reads from `GET /backends`. */
export interface BackendSummary {
  name: string;
  backend_type: string;
  path: string | null;
  endpoint: string | null;
  region: string | null;
  force_path_style: boolean | null;
  is_synthesized?: boolean;
}

/** The four configuration sections, as `GET /config/section/:name` returns them. */
export type ConfigSections = Partial<Record<'admission' | 'access' | 'storage' | 'advanced', unknown>>;

/** Label of the setting a configuration path belongs to (first prefix wins). */
const LOST_LABELS: [prefix: string, label: string][] = [
  ['admission', 'request rules'],
  ['access.iam_mode', 'the declarative IAM mode'],
  ['access.iam_users', 'users, groups and sign-in providers declared in the file'],
  ['access.iam_groups', 'users, groups and sign-in providers declared in the file'],
  ['access.auth_providers', 'users, groups and sign-in providers declared in the file'],
  ['access.group_mapping_rules', 'users, groups and sign-in providers declared in the file'],
  ['access', 'access settings'],
  ['storage.buckets', 'bucket settings'],
  ['storage.replication', 'replication rules'],
  ['storage.lifecycle', 'lifecycle rules'],
  ['storage.backends', 'storage backends'],
  ['storage.default_backend', 'storage backends'],
  ['storage', 'the storage backend settings'],
  ['advanced.event_delivery', 'event delivery'],
  ['advanced.config_sync_bucket', 'the configuration sync bucket'],
  ['advanced.listen_addr', 'the listen address'],
  ['advanced', 'advanced settings'],
];

function leafPaths(value: unknown, path: string, out: string[]): void {
  if (value === null || value === undefined) return;
  if (typeof value === 'object' && !Array.isArray(value)) {
    for (const [k, v] of Object.entries(value)) leafPaths(v, path ? `${path}.${k}` : k, out);
    return;
  }
  if (Array.isArray(value) && value.length === 0) return;
  out.push(path);
}

/**
 * What applying the wizard would throw away. The wizard sends a WHOLE
 * configuration document, so everything the running configuration holds is
 * at stake. The server leaves default values out of each section, so a
 * fresh install has (almost) empty sections; a value set by an environment
 * variable is not in the file and survives, so it does not count. Returns
 * readable labels, deduplicated, in a stable order. Empty = a fresh install.
 */
export function settingsAtRisk(sections: ConfigSections, envYamlPaths: readonly string[]): string[] {
  const leaves: string[] = [];
  for (const [name, doc] of Object.entries(sections)) leafPaths(doc, name, leaves);
  const labels = new Set<string>();
  for (const leaf of leaves) {
    if (envYamlPaths.some((e) => leaf === e || leaf.startsWith(`${e}.`))) continue;
    const hit = LOST_LABELS.find(([prefix]) => leaf === prefix || leaf.startsWith(`${prefix}.`));
    labels.add(hit ? hit[1] : 'other settings');
  }
  const order = [...LOST_LABELS.map(([, l]) => l), 'other settings'];
  return [...labels].sort((a, b) => order.indexOf(a) - order.indexOf(b));
}

export interface ExistingSetup {
  /**
   * True when applying the wizard would lose anything: the running
   * configuration is not the default one (see `settingsAtRisk`).
   */
  configured: boolean;
  /** Readable names of the settings that applying the wizard would replace. */
  atRisk: string[];
  /** Type of the default backend (or the only one), if known. */
  kind: 'filesystem' | 's3' | null;
  fsPath: string;
  s3Endpoint: string;
  s3Region: string;
  s3ForcePathStyle: boolean;
}

export function describeExistingSetup(
  backends: BackendSummary[],
  atRisk: string[],
  defaultBackend: string | null,
): ExistingSetup {
  const current =
    backends.find((b) => b.name === defaultBackend) ?? (backends.length > 0 ? backends[0] : undefined);
  const type = current?.backend_type;
  return {
    // Fail safe: anything but the default configuration is at risk.
    configured: atRisk.length > 0,
    atRisk,
    kind: type === 's3' ? 's3' : type === 'filesystem' ? 'filesystem' : null,
    fsPath: type === 'filesystem' ? current?.path ?? '' : '',
    s3Endpoint: type === 's3' ? current?.endpoint ?? '' : '',
    s3Region: type === 's3' ? current?.region ?? 'us-east-1' : 'us-east-1',
    s3ForcePathStyle: type === 's3' ? current?.force_path_style ?? true : true,
  };
}
