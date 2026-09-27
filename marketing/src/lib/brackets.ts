// brackets.ts — single source of truth for the pricing model.
//
// This module feeds BOTH the calculator React island AND the static
// tier cards on /pricing. Any tuning lands here once.
//
// The model (licensing 2.0): production use is free while the
// organization's Stored Footprint stays at or under 15 TB; above it, one
// flat Commercial plan per organization, up to 1 PB; above 1 PB, or for
// custom terms, Enterprise (talk to sales). OEM covers hosted services and
// embedding in a product. No per-TB brackets, no separate support product.
//
// USD only.

/** The BUSL-1.1 Additional Use Grant threshold, in TB of Stored Footprint:
 * every copy DeltaGlider writes (primary, replicas, archives, references),
 * after compression, summed over the whole organization. */
export const FREE_GRANT_TB = 15;

/** Above this Stored Footprint (1 PB), the Enterprise tier applies. */
export const ENTERPRISE_FOOTPRINT_TB = 1000;

/** The flat Commercial plan price, USD per year per organization. */
export const COMMERCIAL_PRICE_USD = 5_000;

/** A pricing tier — one card on /pricing + one Offer in JSON-LD. */
export interface Bracket {
  /** Stable identifier used as the SKU in schema.org Offers + URL anchor. */
  id: 'free' | 'trial' | 'commercial' | 'enterprise' | 'oem';
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
    priceLabel: '$0',
    description:
      'You get the full product and host it yourself, with community support on GitHub. Production use is free up to 15 TB of compressed stored data per organization, counting every copy that DeltaGlider writes. Nothing is held back and there are no license keys. Every release becomes Apache-2.0 two years after it ships.',
  },
  {
    id: 'trial',
    name: 'Commercial trial',
    priceUsd: 0,
    priceLabel: 'Free, 30 days',
    description:
      'For 30 days you get the full Commercial plan: direct engineering email, a 12h response SLA, and one architecture review call. See /trial.',
  },
  {
    id: 'commercial',
    name: 'Commercial',
    priceUsd: COMMERCIAL_PRICE_USD,
    priceLabel: '$5k/year',
    description:
      'The price is per organization, with unlimited instances, clusters and regions. It includes everything: use beyond the 15 TB grant up to 1 PB, direct engineering email with a 12h business-hours response SLA, signed builds with an SBOM, a CVE response commitment, and every new feature as it ships.',
  },
  {
    id: 'enterprise',
    name: 'Enterprise',
    priceUsd: null,
    priceLabel: 'Talk to sales',
    description:
      'This plan is for an organization whose stored footprint is above 1 PB, or that needs custom terms such as indemnity, a security review, or procurement paperwork. We agree the price and the terms with you.',
  },
  {
    id: 'oem',
    name: 'OEM & embedding',
    priceUsd: null,
    priceLabel: 'Talk to sales',
    description:
      'This plan is for two cases. The first is offering DeltaGlider, or a service whose primary value comes from it, to third parties as a hosted, managed or multi-tenant service. The second is shipping DeltaGlider inside or with a commercial product or appliance: the OEM license then covers your customers\' use of the copy you ship them. The price depends on your use case.',
  },
] as const;

/** Tiers eligible to surface as schema.org Offers — fixed-price paid ones only.
 * Excludes the trial ($0 — handled by trialOfferSchema separately) and the
 * non-priced Enterprise and OEM rows. */
export const SCHEMA_OFFER_BRACKETS = BRACKETS.filter(
  (b) => b.priceUsd !== null && b.priceUsd > 0,
);

/** Which tier applies to a given Stored Footprint in TB (every stored copy,
 * after compression): at or under the grant → Free; up to 1 PB → the flat
 * Commercial plan; above 1 PB → Enterprise. */
export function bracketForFootprintTb(storedFootprintTb: number): Bracket {
  const id =
    storedFootprintTb <= FREE_GRANT_TB
      ? 'free'
      : storedFootprintTb <= ENTERPRISE_FOOTPRINT_TB
        ? 'commercial'
        : 'enterprise';
  return BRACKETS.find((b) => b.id === id)!;
}
