// brackets.ts — single source of truth for the pricing model.
//
// This module feeds BOTH the calculator React island AND the static
// tier cards on /pricing. Any tuning lands here once.
//
// The model (since the BUSL-1.1 relicense): production use is free
// under the license's 15 TB compressed-footprint grant; above it, one
// flat Commercial plan. No TB brackets, no separate support product.
//
// USD only.

/** The BUSL-1.1 Additional Use Grant threshold, in TB of COMPRESSED
 * stored footprint (the bytes physically stored, after compression). */
export const FREE_GRANT_TB = 15;

/** The flat Commercial plan price, USD per year per production deployment. */
export const COMMERCIAL_PRICE_USD = 5_000;

/** A pricing tier — one card on /pricing + one Offer in JSON-LD. */
export interface Bracket {
  /** Stable identifier used as the SKU in schema.org Offers + URL anchor. */
  id: 'free' | 'trial' | 'commercial' | 'oem';
  /** Display name. */
  name: string;
  /**
   * Annual price in USD as a NUMBER for the calculator math.
   * Use null for "talk to sales"; 0 for free rows.
   * (priceLabel below carries the displayed string.)
   */
  priceUsd: number | null;
  /** Price as shown to humans. */
  priceLabel: string;
  /** What the customer gets — shown on the card and in JSON-LD `description`. */
  description: string;
}

/** Tiers in display order (top → bottom of the pricing page). */
export const BRACKETS: readonly Bracket[] = [
  {
    id: 'free',
    name: 'Free',
    priceUsd: 0,
    priceLabel: 'Free, BUSL-1.1',
    description:
      'The full product, self-hosted, with community support on GitHub. Production use is free up to 15 TB of compressed stored data per organization. Nothing is gated and there are no license keys. Every release becomes Apache-2.0 two years after it ships.',
  },
  {
    id: 'trial',
    name: 'Commercial trial',
    priceUsd: 0,
    priceLabel: 'Free, 30 days',
    description:
      'The full Commercial plan for 30 days: direct engineering email, 12h response SLA, one architecture review call. See /trial.',
  },
  {
    id: 'commercial',
    name: 'Commercial',
    priceUsd: COMMERCIAL_PRICE_USD,
    priceLabel: '$5k/year',
    description:
      'Per production deployment, everything included: use beyond the 15 TB grant, direct engineering email with a 12h business-hours response SLA, signed builds with an SBOM, a CVE response commitment, and every new capability as it ships.',
  },
  {
    id: 'oem',
    name: 'OEM & embedding',
    priceUsd: null,
    priceLabel: 'Talk to sales',
    description:
      'For embedding DeltaGlider in a proprietary product, or offering it to your customers as a hosted or managed service. Pricing depends on use case, not footprint.',
  },
] as const;

/** Tiers eligible to surface as schema.org Offers — fixed-price paid ones only.
 * Excludes the trial ($0 — handled by trialOfferSchema separately) and the
 * non-priced OEM row. */
export const SCHEMA_OFFER_BRACKETS = BRACKETS.filter(
  (b) => b.priceUsd !== null && b.priceUsd > 0,
);

/** Which tier applies to a given compressed stored footprint in TB:
 * under the license grant → Free; above it → the flat Commercial plan. */
export function bracketForFootprintTb(storedFootprintTb: number): Bracket {
  const id = storedFootprintTb <= FREE_GRANT_TB ? 'free' : 'commercial';
  return BRACKETS.find((b) => b.id === id)!;
}
