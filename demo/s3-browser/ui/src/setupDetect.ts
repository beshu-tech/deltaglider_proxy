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

export interface ExistingSetup {
  /** True when the proxy has named backends or at least one bucket. */
  configured: boolean;
  backendCount: number;
  bucketCount: number;
  /** Type of the default backend (or the only one), if known. */
  kind: 'filesystem' | 's3' | null;
  fsPath: string;
  s3Endpoint: string;
  s3Region: string;
  s3ForcePathStyle: boolean;
}

export function describeExistingSetup(
  backends: BackendSummary[],
  bucketCount: number,
  defaultBackend: string | null,
): ExistingSetup {
  const named = backends.filter((b) => !b.is_synthesized);
  const current =
    backends.find((b) => b.name === defaultBackend) ?? (backends.length > 0 ? backends[0] : undefined);
  const type = current?.backend_type;
  return {
    // A fresh install shows only the synthesized singleton backend and no
    // buckets. Anything more is an operator's work that the wizard would
    // overwrite.
    configured: named.length > 0 || bucketCount > 0,
    backendCount: backends.length,
    bucketCount,
    kind: type === 's3' ? 's3' : type === 'filesystem' ? 'filesystem' : null,
    fsPath: type === 'filesystem' ? current?.path ?? '' : '',
    s3Endpoint: type === 's3' ? current?.endpoint ?? '' : '',
    s3Region: type === 's3' ? current?.region ?? 'us-east-1' : 'us-east-1',
    s3ForcePathStyle: type === 's3' ? current?.force_path_style ?? true : true,
  };
}
