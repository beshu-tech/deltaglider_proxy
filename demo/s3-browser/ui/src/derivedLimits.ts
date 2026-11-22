import { formatBytes } from './utils';

/** One limit of `GET /config` `limits`. */
interface LimitValue {
  bytes: number;
  /** `auto`, `explicit`, `env` or `invalid-auto`. */
  source: string;
  /** How the value was found, in words. */
  detail: string;
}

/** The `limits` object of `GET /config`: what the proxy derived from the
 *  spool disk at start. Fields that an older server omits are optional. */
export interface DerivedLimits {
  spool_dir?: string;
  free_bytes?: number;
  spool_budget?: LimitValue;
  per_upload?: LimitValue;
  total_multipart?: LimitValue;
  max_object_size: LimitValue;
  delta_ceiling?: LimitValue;
  delta_ceiling_encrypted?: LimitValue;
  reference_cache?: LimitValue;
  warnings?: string[];
}

export interface DerivedLimitRow {
  id: string;
  label: string;
  /** The value for a person: "8.0 GiB", "no cap", "off". */
  value: string;
  /** Where the value comes from and how it was found. */
  help: string;
  /** The variable that sets the value; none for a value that is only derived. */
  envVar?: string;
}

interface RowSpec {
  id: keyof DerivedLimits;
  label: string;
  envVar?: string;
  /** What a value of 0 means. */
  zero?: string;
}

const ROWS: RowSpec[] = [
  { id: 'spool_budget', label: 'Spool budget', envVar: 'DGP_SPOOL_MAX_BYTES' },
  { id: 'per_upload', label: 'One multipart upload', envVar: 'DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES', zero: 'no cap' },
  { id: 'total_multipart', label: 'All open multipart uploads', envVar: 'DGP_MAX_TOTAL_MULTIPART_BYTES', zero: 'none admitted' },
  { id: 'max_object_size', label: 'Largest object', envVar: 'DGP_MAX_OBJECT_SIZE' },
  { id: 'delta_ceiling', label: 'Delta ceiling' },
  { id: 'delta_ceiling_encrypted', label: 'Delta ceiling (encrypted bucket, multipart upload)' },
  { id: 'reference_cache', label: 'Reference cache', envVar: 'DGP_REFERENCE_CACHE_MAX_BYTES', zero: 'off' },
];

/** The rows of the "Space limits" card, in the order of their derivation.
 *  A limit that the server did not send has no row. */
export function derivedLimitRows(limits: DerivedLimits): DerivedLimitRow[] {
  return ROWS.flatMap((spec) => {
    const limit = limits[spec.id] as LimitValue | undefined;
    if (!limit) return [];
    const value = limit.bytes === 0 && spec.zero ? spec.zero : formatBytes(limit.bytes);
    // The detail of an auto value already says "auto".
    const help = limit.source === 'auto' ? limit.detail : `${limit.source}: ${limit.detail}`;
    return [{ id: spec.id, label: spec.label, value, help, envVar: spec.envVar }];
  });
}
