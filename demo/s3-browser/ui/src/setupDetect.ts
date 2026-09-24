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

/** What the proxy's own configuration holds (from `GET /config`). */
export interface ConfigCounts {
  /** Bucket settings entries (`storage.buckets`). */
  bucketSettings: number;
  /** Request rules (`admission.blocks`). */
  requestRules: number;
}

export interface ExistingSetup {
  /**
   * True when the proxy's configuration holds an operator's work: named
   * backends, bucket settings, or request rules. Buckets that merely exist
   * on the storage do not count: the wizard does not touch them.
   */
  configured: boolean;
  /** Named backends (the synthesized singleton is not counted). */
  backendCount: number;
  bucketSettingsCount: number;
  requestRuleCount: number;
  /** Type of the default backend (or the only one), if known. */
  kind: 'filesystem' | 's3' | null;
  fsPath: string;
  s3Endpoint: string;
  s3Region: string;
  s3ForcePathStyle: boolean;
}

export function describeExistingSetup(
  backends: BackendSummary[],
  counts: ConfigCounts,
  defaultBackend: string | null,
): ExistingSetup {
  const named = backends.filter((b) => !b.is_synthesized);
  const current =
    backends.find((b) => b.name === defaultBackend) ?? (backends.length > 0 ? backends[0] : undefined);
  const type = current?.backend_type;
  return {
    // A fresh install shows only the synthesized singleton backend, with no
    // bucket settings and no request rules. Anything more is an operator's
    // work that the wizard would overwrite.
    configured: named.length > 0 || counts.bucketSettings > 0 || counts.requestRules > 0,
    backendCount: named.length,
    bucketSettingsCount: counts.bucketSettings,
    requestRuleCount: counts.requestRules,
    kind: type === 's3' ? 's3' : type === 'filesystem' ? 'filesystem' : null,
    fsPath: type === 'filesystem' ? current?.path ?? '' : '',
    s3Endpoint: type === 's3' ? current?.endpoint ?? '' : '',
    s3Region: type === 's3' ? current?.region ?? 'us-east-1' : 'us-east-1',
    s3ForcePathStyle: type === 's3' ? current?.force_path_style ?? true : true,
  };
}
