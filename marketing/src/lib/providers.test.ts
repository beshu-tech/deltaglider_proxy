import { describe, it, expect } from 'vitest';
import {
  PROVIDERS,
  getProvider,
  monthlyCostUsd,
  effectiveUsdPerGbMonth,
  headlineUsdPerGbMonth,
} from './providers';

describe('providers', () => {
  it('exposes a non-empty, id-unique provider list', () => {
    expect(PROVIDERS.length).toBeGreaterThan(5);
    const ids = PROVIDERS.map((p) => p.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it('flat pay-as-you-go scales linearly (AWS S3 $0.023/GB)', () => {
    expect(monthlyCostUsd('s3', 1000)).toBeCloseTo(23, 5);
    expect(monthlyCostUsd('s3', 2000)).toBeCloseTo(46, 5);
  });

  it('free tier: B2 bills nothing for the first 10 GB', () => {
    expect(monthlyCostUsd('b2', 10)).toBe(0);
    expect(monthlyCostUsd('b2', 110)).toBeCloseTo(100 * 0.006, 6); // (110-10)*0.006
  });

  it('minimum threshold: Wasabi floors at 1 TB even for tiny footprints', () => {
    // 100 GB still billed as 1000 GB.
    expect(monthlyCostUsd('wasabi', 100)).toBeCloseTo(1000 * 0.00799, 5);
    // Above the floor, marginal rate applies.
    expect(monthlyCostUsd('wasabi', 2000)).toBeCloseTo(2000 * 0.00799, 5);
  });

  it('normalises EUR-priced providers to USD (Hetzner)', () => {
    // 1 TB floor, €0.00649/GB → €6.49 → ×1.08 ≈ $7.01
    expect(monthlyCostUsd('hetzner', 1000)).toBeCloseTo(1000 * 0.00649 * 1.08, 4);
  });

  it('effective $/GB inflates BELOW a minimum (the floor effect)', () => {
    // At 100 GB, Wasabi bills the 1 TB floor → effective rate is 10× the headline.
    const eff = effectiveUsdPerGbMonth('wasabi', 100);
    expect(eff).toBeCloseTo((1000 * 0.00799) / 100, 5); // floor cost / actual gb
    expect(eff).toBeGreaterThan(headlineUsdPerGbMonth('wasabi'));
  });

  it('effective $/GB converges to the headline rate ABOVE the minimum', () => {
    expect(effectiveUsdPerGbMonth('wasabi', 50_000)).toBeCloseTo(
      headlineUsdPerGbMonth('wasabi'),
      6,
    );
  });

  it('getProvider falls back to the first provider for unknown ids', () => {
    expect(getProvider('does-not-exist').id).toBe(PROVIDERS[0].id);
  });

  it('archive tiers are the cheapest per GB (Glacier Deep)', () => {
    const deep = headlineUsdPerGbMonth('glacier-deep');
    const s3 = headlineUsdPerGbMonth('s3');
    expect(deep).toBeLessThan(s3);
  });
});
