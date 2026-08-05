// nav.ts — single source of truth for site navigation links.
// Consumed by SiteHeader (primary nav) and SiteFooter (link columns);
// add or rename a destination here once and both surfaces follow.

export interface NavLink {
  label: string;
  href: string;
}

/** Primary header navigation, in display order. */
export const NAV_LINKS: readonly NavLink[] = [
  { label: 'SaaS', href: '/saas' },
  { label: 'Regulated', href: '/regulated' },
  { label: 'Pricing', href: '/pricing' },
  { label: 'Docs', href: '/docs' },
  { label: 'Trial', href: '/trial' },
  { label: 'GitHub', href: 'https://github.com/beshu-tech/deltaglider_proxy' },
];

export interface FooterColumn {
  title: string;
  links: readonly NavLink[];
}

/** Footer link columns, in display order. */
export const FOOTER_COLUMNS: readonly FooterColumn[] = [
  {
    title: 'Product',
    links: [
      { label: 'For SaaS', href: '/saas' },
      { label: 'For regulated', href: '/regulated' },
      { label: 'Pricing', href: '/pricing' },
      { label: '30-day trial', href: '/trial' },
      { label: 'Case studies', href: '/case-studies' },
    ],
  },
  {
    title: 'Resources',
    links: [
      { label: 'GitHub', href: 'https://github.com/beshu-tech/deltaglider_proxy' },
      { label: 'Documentation', href: '/docs' },
      { label: 'Quickstart', href: '/docs/tutorials/first-delta-savings' },
      { label: 'Reference docs', href: '/docs/reference' },
      { label: 'Contact', href: 'mailto:contact@beshu.tech?subject=DeltaGlider%20-%20General%20inquiry' },
    ],
  },
  {
    title: 'Beshu Tech',
    links: [
      { label: 'beshu.tech', href: 'https://beshu.tech' },
      { label: 'ReadonlyREST', href: 'https://readonlyrest.com' },
      { label: 'Anaphora', href: 'https://anaphora.beshu.tech' },
    ],
  },
];
