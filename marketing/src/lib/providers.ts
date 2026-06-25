// providers.ts — object-storage provider pricing models.
//
// Pure, unit-testable. Real-world pricing is NOT a flat $/GB: providers add
// free tiers (B2's first 10 GB), and — crucially for DeltaGlider — minimum
// billing thresholds (a 1 TB floor). That floor is exactly why the comparison
// matters: compressing 30 TB → 3 TB saves a lot, but on a 1 TB-minimum provider
// the post-compression bill can't drop below the floor.
//
// All costs are normalised to USD/month for apples-to-apples comparison; the
// `currency` field is kept so the UI can tag the €-priced ones honestly.

/** Fixed EUR→USD used only to normalise the EUR-priced providers for the
 *  comparison. Approximate; the UI notes which providers are €-native. */
const EUR_USD = 1.08;

export interface Provider {
  id: string;
  name: string;
  /** Currency the provider actually bills in. */
  currency: 'USD' | 'EUR';
  /** One-line caveat shown under the provider. */
  notes: string;
  /** Whether this row should be flagged as "archive / retrieval-delayed". */
  archive?: boolean;
  /** Monthly cost in the provider's NATIVE currency for `gb` of stored data. */
  nativeMonthlyCost: (gb: number) => number;
}

/** Min-billable-threshold helper: bills for at least `floorGb` GB. */
const withFloor = (floorGb: number, perGb: number) => (gb: number) =>
  Math.max(floorGb, gb) * perGb;

/** Free-tier helper: first `freeGb` GB free, pay-as-you-go after. */
const withFreeTier = (freeGb: number, perGb: number) => (gb: number) =>
  Math.max(0, gb - freeGb) * perGb;

/** Flat pay-as-you-go per GB. */
const flat = (perGb: number) => (gb: number) => gb * perGb;

export const PROVIDERS: Provider[] = [
  {
    id: 'b2',
    name: 'Backblaze B2',
    currency: 'USD',
    notes: 'First 10 GB free · true pay-as-you-go',
    nativeMonthlyCost: withFreeTier(10, 0.006),
  },
  {
    id: 'idrive',
    name: 'iDrive e2',
    currency: 'USD',
    notes: '1 TB minimum billing',
    nativeMonthlyCost: withFloor(1000, 0.006),
  },
  {
    id: 'hetzner',
    name: 'Hetzner Object Storage',
    currency: 'EUR',
    notes: '1 TB minimum · €-priced',
    nativeMonthlyCost: withFloor(1000, 0.00649),
  },
  {
    id: 'wasabi',
    name: 'Wasabi',
    currency: 'USD',
    notes: '1 TB minimum billing',
    nativeMonthlyCost: withFloor(1000, 0.00799),
  },
  {
    id: 'r2',
    name: 'Cloudflare R2',
    currency: 'USD',
    notes: 'First 10 GB free · zero egress fees',
    nativeMonthlyCost: withFreeTier(10, 0.015),
  },
  {
    id: 's3',
    name: 'AWS S3 Standard',
    currency: 'USD',
    notes: 'First 50 TB tier · egress + API extra',
    nativeMonthlyCost: flat(0.023),
  },
  {
    id: 'gcs',
    name: 'GCP Standard',
    currency: 'USD',
    notes: 'us-central1 · egress extra',
    nativeMonthlyCost: flat(0.02),
  },
  {
    id: 'azure',
    name: 'Azure Blob Hot',
    currency: 'USD',
    notes: 'East US · egress extra',
    nativeMonthlyCost: flat(0.0184),
  },
  {
    id: 'glacier-flex',
    name: 'S3 Glacier Flexible',
    currency: 'USD',
    notes: 'Retrieval fees + delays apply',
    archive: true,
    nativeMonthlyCost: flat(0.0036),
  },
  {
    id: 'glacier-deep',
    name: 'S3 Glacier Deep Archive',
    currency: 'USD',
    notes: '180-day min · retrieval up to 12 h',
    archive: true,
    nativeMonthlyCost: flat(0.00099),
  },
];

const PROVIDER_BY_ID: Record<string, Provider> = Object.fromEntries(
  PROVIDERS.map((p) => [p.id, p]),
);

export function getProvider(id: string): Provider {
  return PROVIDER_BY_ID[id] ?? PROVIDERS[0];
}

/** Monthly cost in USD for `gb` on a provider, normalising EUR→USD. */
export function monthlyCostUsd(id: string, gb: number): number {
  const p = getProvider(id);
  const native = p.nativeMonthlyCost(Math.max(0, gb));
  return p.currency === 'EUR' ? native * EUR_USD : native;
}

/** The EFFECTIVE blended $/GB-month at a given footprint — lets the existing
 *  linear `calculate()` consume a provider without changing its tested math.
 *  (At/above any minimum or free-tier boundary this is the marginal rate; below
 *  it, the floor inflates it — which is the honest picture.) */
export function effectiveUsdPerGbMonth(id: string, gb: number): number {
  const g = Math.max(1, gb);
  return monthlyCostUsd(id, g) / g;
}

/** Convenience: USD per GB-month at a "large" footprint (the headline rate the
 *  provider advertises, free of floor distortion) — used for the picker labels. */
export function headlineUsdPerGbMonth(id: string): number {
  return monthlyCostUsd(id, 100_000) / 100_000;
}
