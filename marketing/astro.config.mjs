// @ts-check
import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';
import react from '@astrojs/react';

// 301 redirects. Two generations of moves live here:
//  - the legacy marketing URLs (v5 plan §6.1): /artifact-storage,
//    /minio-migration, /s3-to-hetzner-wasabi → /saas; /multi-cloud-control-plane
//    → /regulated.
//  - the Diátaxis docs restructure (2026-06): every pre-restructure docs URL
//    301s to its new home. /docs/faq kept its slug (42-faq.md → faq.md).
// The sitemap filter below derives from this map — add a redirect, and its
// stub page is excluded from the sitemap automatically.
const REDIRECTS = {
  '/docs/quickstart': '/docs/tutorials/first-delta-savings',
  '/docs/first-bucket': '/docs/how-to/route-a-bucket-to-a-backend',
  '/docs/production-deployment': '/docs/how-to/go-to-production',
  '/docs/production-security-checklist': '/docs/tutorials/secure-your-proxy',
  '/docs/upgrade-guide': '/docs/how-to/upgrade',
  '/docs/kubernetes-helm': '/docs/how-to/deploy-on-kubernetes',
  '/docs/docker-compose': '/docs/how-to/deploy-with-docker-compose',
  '/docs/monitoring-and-alerts': '/docs/how-to/monitor-with-prometheus',
  '/docs/troubleshooting': '/docs/how-to/troubleshooting',
  '/docs/auth/oauth-setup': '/docs/how-to/set-up-sso',
  '/docs/auth/sigv4-and-iam': '/docs/how-to/create-iam-users',
  '/docs/auth/iam-conditions': '/docs/how-to/restrict-access-with-conditions',
  '/docs/auth/rate-limiting': '/docs/reference/rate-limits',
  '/docs/reference/how-delta-works': '/docs/explanation/delta-compression',
  '/docs/reference/encryption-at-rest': '/docs/explanation/encryption-at-rest',
  '/artifact-storage': '/saas',
  '/minio-migration': '/saas',
  '/s3-to-hetzner-wasabi': '/saas',
  '/multi-cloud-control-plane': '/regulated',
};

// https://docs.astro.build/en/reference/configuration-reference/
export default defineConfig({
  site: 'https://deltaglider.com',

  // Static-first; no server runtime needed for a marketing site.
  output: 'static',

  // Build to dist/ — matches the existing GH Pages workflow
  // (.github/workflows/marketing-pages.yml uploads marketing/dist).
  build: {
    format: 'directory', // /saas/ instead of /saas.html
  },

  redirects: REDIRECTS,

  // Integrations
  integrations: [
    // React: used only by the pricing calculator island.
    // Most pages are pure Astro (zero JS). The calculator hydrates
    // client-side via client:visible — see /pricing.
    react(),

    // /sitemap-index.xml + /sitemap-0.xml — referenced by /robots.txt.
    // Excludes every redirect stub (no value in indexing pages that
    // meta-refresh to another URL).
    sitemap({
      filter: (page) => {
        const path = new URL(page).pathname.replace(/\/$/, '');
        return !(path in REDIRECTS);
      },
    }),
  ],
});
