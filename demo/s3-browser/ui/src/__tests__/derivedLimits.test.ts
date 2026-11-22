import { describe, expect, it } from 'vitest';
import { derivedLimitRows } from '../derivedLimits';

const GIB = 1024 ** 3;

describe('derivedLimitRows', () => {
  it('shows every derived limit with its value, source and reason', () => {
    const rows = derivedLimitRows({
      spool_budget: { bytes: 30 * GIB, source: 'auto', detail: 'auto: 50% of 60 GiB free' },
      per_upload: { bytes: 0, source: 'env', detail: 'set by DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES: no cap' },
      total_multipart: { bytes: 15 * GIB, source: 'auto', detail: 'auto: half of the spool budget' },
      max_object_size: { bytes: 512 * 1024 * 1024, source: 'explicit', detail: 'set by max_object_size' },
      delta_ceiling: { bytes: 10 * GIB, source: 'auto', detail: 'the spool budget divided by 3' },
      reference_cache: { bytes: 8 * GIB, source: 'auto', detail: 'reference cache 8 GiB (auto: 16 references)' },
    });
    expect(rows.map((r) => r.id)).toEqual([
      'spool_budget',
      'per_upload',
      'total_multipart',
      'max_object_size',
      'delta_ceiling',
      'reference_cache',
    ]);
    const byId = Object.fromEntries(rows.map((r) => [r.id, r]));
    expect(byId.spool_budget.value).toBe('30.0 GiB');
    expect(byId.spool_budget.help).toBe('auto: 50% of 60 GiB free');
    expect(byId.per_upload.value).toBe('no cap');
    expect(byId.max_object_size.help).toBe('explicit: set by max_object_size');
    expect(byId.delta_ceiling.envVar).toBeUndefined();
    expect(byId.reference_cache.envVar).toBe('DGP_REFERENCE_CACHE_MAX_BYTES');
  });

  it('says off for a reference cache of 0, and skips what an older server omits', () => {
    const rows = derivedLimitRows({
      max_object_size: { bytes: GIB, source: 'auto', detail: 'auto: the per-upload limit' },
      reference_cache: { bytes: 0, source: 'env', detail: 'reference cache off' },
    });
    expect(rows.map((r) => `${r.id}=${r.value}`)).toEqual(['max_object_size=1.0 GiB', 'reference_cache=off']);
  });
});
