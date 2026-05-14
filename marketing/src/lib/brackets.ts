// brackets.ts — single source of truth for production-support pricing.
//
// Per v5 plan §5.6: this module feeds BOTH the calculator React island
// AND the static <table> on /pricing. Any tuning lands here once.
//
// USD only — v5 plan §5.2 rejects EUR/multi-currency for now.

/** A pricing bracket — one row in the public table + one Offer in JSON-LD. */
export interface Bracket {
  /** Stable identifier used as the SKU in schema.org Offers + URL anchor. */
  id: 'starter' | 'growth' | 'scale' | 'enterprise' | 'oss' | 'trial' | 'commercial';
  /** Display name. Long form; the table column "Tier". */
  name: string;
  /**
   * Annual price in USD as a NUMBER for the calculator math.
   * Use null for "talk to sales" / "free" / non-priced rows.
   * (priceLabel below carries the displayed string.)
   */
  priceUsd: number | null;
  /** Price as shown to humans in the table. */
  priceLabel: string;
  /** What the customer gets — shown in the table and in JSON-LD `description`. */
  description: string;
  /**
   * Stored-footprint range in TB.
   * `[min, max]` where max=null means "unbounded above" (Enterprise).
   * undefined for non-bracket rows (OSS, trial, commercial license).
   */
  rangeTb?: [number, number | null];
}

/** Brackets in display order (top → bottom of the pricing table). */
export const BRACKETS: readonly Bracket[] = [
  {
    id: 'oss',
    name: 'Open Source',
    priceUsd: 0,
    priceLabel: 'Free, GPL-3.0',
    description:
      'Full product. No footprint cap. Self-host. Community Slack + GitHub.',
  },
  {
    id: 'trial',
    name: 'Production support trial',
    priceUsd: 0,
    priceLabel: 'Free, 30 days',
    description:
      'Direct engineering Slack, response SLA, one architecture review call. See /trial.',
  },
  {
    id: 'starter',
    name: 'Production support — Starter',
    priceUsd: 10_000,
    priceLabel: '$10k/year',
    description:
      'Up to 10 TB stored footprint. Eng Slack + 4h business-hours response SLA.',
    rangeTb: [0, 10],
  },
  {
    id: 'growth',
    name: 'Production support — Growth',
    priceUsd: 30_000,
    priceLabel: '$30k/year',
    description:
      '10–50 TB stored footprint. Same SLA + quarterly review call.',
    rangeTb: [10, 50],
  },
  {
    id: 'scale',
    name: 'Production support — Scale',
    priceUsd: 60_000,
    priceLabel: '$60k/year',
    description:
      '50–250 TB stored footprint. 1h response SLA, monthly review, named eng contact.',
    rangeTb: [50, 250],
  },
  {
    id: 'enterprise',
    name: 'Production support — Enterprise',
    priceUsd: null,
    priceLabel: 'Talk to sales',
    description:
      '250 TB+, multi-region, custom SLA, dedicated channel.',
    rangeTb: [250, null],
  },
  {
    id: 'commercial',
    name: 'Commercial license',
    priceUsd: null,
    priceLabel: 'Talk to sales',
    description:
      'For embedding DeltaGlider in a proprietary product, or removing the GPL-3.0 obligation. Pricing depends on use case, not footprint.',
  },
] as const;

/** Brackets that have a stored-footprint range (the priced production-support tiers). */
export const PRICED_BRACKETS = BRACKETS.filter((b): b is Bracket & { rangeTb: [number, number | null] } =>
  b.rangeTb !== undefined,
);

/** Brackets eligible to surface as schema.org Offers — fixed-price ones only.
 * Excludes the trial ($0 — handled by trialOfferSchema separately) and the
 * non-priced rows (Enterprise / Commercial license). */
export const SCHEMA_OFFER_BRACKETS = BRACKETS.filter(
  (b) => b.priceUsd !== null && b.priceUsd > 0 && b.rangeTb !== undefined,
);

/** Find the bracket whose range contains the given stored footprint in TB.
 * Returns Enterprise for footprints above the highest priced range. */
export function bracketForFootprintTb(storedFootprintTb: number): Bracket {
  for (const b of PRICED_BRACKETS) {
    const [min, max] = b.rangeTb;
    if (storedFootprintTb >= min && (max === null || storedFootprintTb < max)) {
      return b;
    }
  }
  // Above the highest priced range → Enterprise
  return BRACKETS.find((b) => b.id === 'enterprise')!;
}
