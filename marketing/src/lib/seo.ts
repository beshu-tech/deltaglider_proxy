// SPDX-License-Identifier: GPL-3.0-only
// seo.ts — single source of truth for SEO + schema.org JSON-LD.
//
// Every page's JSON-LD is composed from these typed builders. Update
// constants here once; every page picks up the change on next build.
//
// Schema reference: https://schema.org/docs/full.html
// Google's structured data: https://developers.google.com/search/docs/appearance/structured-data/intro-structured-data

export const SITE = {
  url: 'https://deltaglider.com',
  name: 'DeltaGlider',
  /** Long product name used in metadata. */
  productName: 'DeltaGlider — S3-compatible storage compression',
  /** Default description for pages that don't override it. */
  description:
    'Storage compression for S3, behind the same S3 API your apps already use. Open source (GPL-3.0). Built by Beshu Tech.',
  /** Path to the default Open Graph preview image (1200x630). */
  ogImage: '/og-default.svg',
  /** Repo URL (also used for sameAs in SoftwareApplication). */
  repoUrl: 'https://github.com/beshu-tech/deltaglider_proxy',
} as const;

export const BESHU = {
  /** Beshu Tech canonical homepage. */
  url: 'https://beshu.tech',
  /** Legal entity name. */
  legalName: 'Beshu Limited',
  /** Short brand name used in display. */
  shortName: 'Beshu Tech',
  /** Founding year — matches the "since 2017" claim across the site. */
  foundingDate: '2017',
  address: {
    streetAddress: '1st Floor, 124 Cleveland Street',
    addressLocality: 'London',
    postalCode: 'W1T 6PH',
    addressCountry: 'GB',
  },
  /** Sister product URLs for sameAs / brand. */
  sisterProducts: {
    readonlyrest: 'https://readonlyrest.com',
    anaphora: 'https://anaphora.beshu.tech',
  },
} as const;

/** Top-level Organization schema — represents Beshu Tech. */
export function organizationSchema() {
  return {
    '@context': 'https://schema.org',
    '@type': 'Organization',
    '@id': `${BESHU.url}#organization`,
    name: BESHU.shortName,
    legalName: BESHU.legalName,
    url: BESHU.url,
    foundingDate: BESHU.foundingDate,
    address: {
      '@type': 'PostalAddress',
      streetAddress: BESHU.address.streetAddress,
      addressLocality: BESHU.address.addressLocality,
      postalCode: BESHU.address.postalCode,
      addressCountry: BESHU.address.addressCountry,
    },
    // sameAs links the organization to its other identities online —
    // tells Google "this entity is also at these URLs."
    sameAs: [
      BESHU.sisterProducts.readonlyrest,
      BESHU.sisterProducts.anaphora,
      SITE.url,
      SITE.repoUrl,
    ],
    // brand: each sister product is a Brand under the same parent.
    // Lets Google attribute reviews / customer references to the
    // correct product line.
    brand: [
      {
        '@type': 'Brand',
        name: 'ReadonlyREST',
        url: BESHU.sisterProducts.readonlyrest,
      },
      {
        '@type': 'Brand',
        name: 'Anaphora',
        url: BESHU.sisterProducts.anaphora,
      },
      {
        '@type': 'Brand',
        name: SITE.name,
        url: SITE.url,
      },
    ],
  };
}

/** WebSite schema for the homepage. Enables Google's sitelinks search box. */
export function webSiteSchema() {
  return {
    '@context': 'https://schema.org',
    '@type': 'WebSite',
    '@id': `${SITE.url}#website`,
    url: SITE.url,
    name: SITE.name,
    description: SITE.description,
    publisher: {
      '@id': `${BESHU.url}#organization`,
    },
    inLanguage: 'en',
  };
}

/** SoftwareApplication schema — describes DeltaGlider itself. */
export function softwareApplicationSchema() {
  return {
    '@context': 'https://schema.org',
    '@type': 'SoftwareApplication',
    '@id': `${SITE.url}#software`,
    name: SITE.name,
    description: SITE.description,
    url: SITE.url,
    applicationCategory: 'DeveloperApplication',
    applicationSubCategory: 'StorageProxy',
    operatingSystem: 'Linux, macOS, Windows (via Docker)',
    // GPL-3.0 + commercial — the OSS price is $0.
    offers: {
      '@type': 'Offer',
      price: '0',
      priceCurrency: 'USD',
      availability: 'https://schema.org/InStock',
      url: SITE.repoUrl,
    },
    publisher: {
      '@id': `${BESHU.url}#organization`,
    },
    softwareRequirements: 'Linux x86_64 / aarch64; Docker recommended',
    license: 'https://www.gnu.org/licenses/gpl-3.0.en.html',
    codeRepository: SITE.repoUrl,
    programmingLanguage: 'Rust',
  };
}

interface WebPageInput {
  /** Page path starting with /, e.g. "/saas". */
  path: string;
  /** Page title — should match <title>. */
  title: string;
  /** Page description — should match <meta description>. */
  description: string;
  /** Optional override for date last modified (ISO 8601). */
  dateModified?: string;
}

/** Generic WebPage schema for any subpage. */
export function webPageSchema({ path, title, description, dateModified }: WebPageInput) {
  const url = path === '/' ? SITE.url : `${SITE.url}${path}`;
  return {
    '@context': 'https://schema.org',
    '@type': 'WebPage',
    '@id': `${url}#webpage`,
    url,
    name: title,
    description,
    isPartOf: { '@id': `${SITE.url}#website` },
    inLanguage: 'en',
    ...(dateModified ? { dateModified } : {}),
  };
}

interface BreadcrumbItem {
  name: string;
  path: string;
}

/** BreadcrumbList — enables Google to show nav structure in search. */
export function breadcrumbListSchema(items: BreadcrumbItem[]) {
  return {
    '@context': 'https://schema.org',
    '@type': 'BreadcrumbList',
    itemListElement: items.map((item, idx) => ({
      '@type': 'ListItem',
      position: idx + 1,
      name: item.name,
      item: item.path === '/' ? SITE.url : `${SITE.url}${item.path}`,
    })),
  };
}

interface PricingTier {
  name: string;
  /** Price in USD as a string, or "0" for free. Use "" for "talk-to-sales". */
  price: string;
  /** Short description shown in search results. */
  description: string;
  /** SKU-ish identifier — unique per tier. */
  sku: string;
}

/** Product schema with multiple Offers — one per pricing tier.
 * Enables Google to surface price ranges in search results. */
export function pricingProductSchema(tiers: PricingTier[]) {
  return {
    '@context': 'https://schema.org',
    '@type': 'Product',
    '@id': `${SITE.url}/pricing#product`,
    name: SITE.name,
    description: 'DeltaGlider production support, scaling with your stored footprint.',
    brand: {
      '@type': 'Brand',
      name: SITE.name,
    },
    offers: tiers
      .filter((t) => t.price !== '') // skip "talk to sales" rows — no fixed price
      .map((t) => ({
        '@type': 'Offer',
        '@id': `${SITE.url}/pricing#${t.sku}`,
        name: t.name,
        description: t.description,
        sku: t.sku,
        price: t.price,
        priceCurrency: 'USD',
        availability: 'https://schema.org/InStock',
        url: `${SITE.url}/pricing`,
        seller: { '@id': `${BESHU.url}#organization` },
      })),
  };
}

interface TrialOfferInput {
  durationDays: number;
}

/** Offer schema for the free trial. */
export function trialOfferSchema({ durationDays }: TrialOfferInput) {
  return {
    '@context': 'https://schema.org',
    '@type': 'Offer',
    '@id': `${SITE.url}/trial#offer`,
    name: `${durationDays}-day DeltaGlider production support trial`,
    description:
      'Direct engineering Slack, response SLA, one architecture review call. The software is GPL-3.0 and remains free regardless.',
    price: '0',
    priceCurrency: 'USD',
    availability: 'https://schema.org/InStock',
    url: `${SITE.url}/trial`,
    eligibleDuration: {
      '@type': 'QuantitativeValue',
      value: durationDays,
      unitCode: 'DAY',
    },
    seller: { '@id': `${BESHU.url}#organization` },
  };
}

interface ArticleInput {
  /** Article URL path. */
  path: string;
  /** Headline — usually the page <h1>. */
  headline: string;
  /** Description / dek. */
  description: string;
  /** Author name (real person). */
  authorName: string;
  /** Optional author URL. */
  authorUrl?: string;
  /** Date published (ISO 8601). */
  datePublished: string;
  /** Date last modified (ISO 8601). Defaults to datePublished. */
  dateModified?: string;
}

/** Article schema for case studies / engineering retrospectives.
 * Enables Google to attribute the piece to a named author in search results. */
export function articleSchema(input: ArticleInput) {
  const url = `${SITE.url}${input.path}`;
  return {
    '@context': 'https://schema.org',
    '@type': 'Article',
    '@id': `${url}#article`,
    headline: input.headline,
    description: input.description,
    url,
    mainEntityOfPage: { '@id': `${url}#webpage` },
    author: {
      '@type': 'Person',
      name: input.authorName,
      ...(input.authorUrl ? { url: input.authorUrl } : {}),
      worksFor: { '@id': `${BESHU.url}#organization` },
    },
    publisher: { '@id': `${BESHU.url}#organization` },
    datePublished: input.datePublished,
    dateModified: input.dateModified ?? input.datePublished,
    inLanguage: 'en',
  };
}
