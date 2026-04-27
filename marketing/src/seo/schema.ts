export const SITE_URL = 'https://deltaglider.com';
export const REPO_URL = 'https://github.com/beshu-tech/deltaglider_proxy';
export const DOCS_PATH = '/docs/';
export const CONTACT_EMAIL = 'deltaglider@beshu.tech';
export const ORG_NAME = 'Beshu Tech';
export const PRODUCT_NAME = 'DeltaGlider Proxy';

export interface OrganizationSchema {
  '@context': 'https://schema.org';
  '@type': 'Organization';
  name: string;
  url: string;
  logo: string;
  sameAs: string[];
}

export interface WebSiteSchema {
  '@context': 'https://schema.org';
  '@type': 'WebSite';
  name: string;
  url: string;
  inLanguage: string;
}

export interface SoftwareApplicationSchema {
  '@context': 'https://schema.org';
  '@type': 'SoftwareApplication';
  name: string;
  description: string;
  applicationCategory: string;
  operatingSystem: string;
  url: string;
  offers: {
    '@type': 'Offer';
    price: '0';
    priceCurrency: 'USD';
  };
  softwareVersion?: string;
  publisher: { '@type': 'Organization'; name: string; url: string };
}

export interface FaqItem {
  question: string;
  answer: string;
}

export interface FaqPageSchema {
  '@context': 'https://schema.org';
  '@type': 'FAQPage';
  mainEntity: Array<{
    '@type': 'Question';
    name: string;
    acceptedAnswer: { '@type': 'Answer'; text: string };
  }>;
}

export interface BreadcrumbListSchema {
  '@context': 'https://schema.org';
  '@type': 'BreadcrumbList';
  itemListElement: Array<{
    '@type': 'ListItem';
    position: number;
    name: string;
    item: string;
  }>;
}

export function organization(): OrganizationSchema {
  return {
    '@context': 'https://schema.org',
    '@type': 'Organization',
    name: ORG_NAME,
    url: 'https://beshu.tech',
    logo: `${SITE_URL}/screenshots/filebrowser.jpg`,
    sameAs: [REPO_URL, 'https://beshu.tech'],
  };
}

export function website(): WebSiteSchema {
  return {
    '@context': 'https://schema.org',
    '@type': 'WebSite',
    name: PRODUCT_NAME,
    url: `${SITE_URL}/`,
    inLanguage: 'en',
  };
}

export function softwareApplication(
  version?: string,
): SoftwareApplicationSchema {
  const base: SoftwareApplicationSchema = {
    '@context': 'https://schema.org',
    '@type': 'SoftwareApplication',
    name: PRODUCT_NAME,
    description:
      'An S3-compatible proxy that reduces storage growth with transparent delta compression, IAM, OAuth, quotas, object replication, metrics, audit, and encryption at rest.',
    applicationCategory: 'DeveloperApplication',
    operatingSystem: 'Linux, macOS',
    url: `${SITE_URL}/`,
    offers: {
      '@type': 'Offer',
      price: '0',
      priceCurrency: 'USD',
    },
    publisher: {
      '@type': 'Organization',
      name: ORG_NAME,
      url: 'https://beshu.tech',
    },
  };
  return version ? { ...base, softwareVersion: version } : base;
}

export function faqPage(items: readonly FaqItem[]): FaqPageSchema {
  return {
    '@context': 'https://schema.org',
    '@type': 'FAQPage',
    mainEntity: items.map((item) => ({
      '@type': 'Question',
      name: item.question,
      acceptedAnswer: {
        '@type': 'Answer',
        text: item.answer,
      },
    })),
  };
}

export function breadcrumb(
  trail: ReadonlyArray<{ name: string; path: string }>,
): BreadcrumbListSchema {
  return {
    '@context': 'https://schema.org',
    '@type': 'BreadcrumbList',
    itemListElement: trail.map((entry, index) => ({
      '@type': 'ListItem',
      position: index + 1,
      name: entry.name,
      item: `${SITE_URL}${entry.path}`,
    })),
  };
}
