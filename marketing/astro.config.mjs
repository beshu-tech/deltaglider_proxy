// @ts-check
import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';
import react from '@astrojs/react';

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

  // 301 redirects from the legacy site's URLs to the new structure.
  // Per the v5 plan §6.1: /artifact-storage, /minio-migration,
  // /s3-to-hetzner-wasabi → /saas; /multi-cloud-control-plane → /regulated.
  redirects: {
    '/artifact-storage': '/saas',
    '/minio-migration': '/saas',
    '/s3-to-hetzner-wasabi': '/saas',
    '/multi-cloud-control-plane': '/regulated',
  },

  // Integrations
  integrations: [
    // React: used only by the pricing calculator island.
    // Most pages are pure Astro (zero JS). The calculator hydrates
    // client-side via client:visible — see /pricing.
    react(),

    // /sitemap-index.xml + /sitemap-0.xml — referenced by /robots.txt.
    // Excludes the legacy-URL redirect stubs (no value in indexing
    // pages that meta-refresh to another URL).
    sitemap({
      filter: (page) =>
        !page.includes('/artifact-storage') &&
        !page.includes('/minio-migration') &&
        !page.includes('/s3-to-hetzner-wasabi') &&
        !page.includes('/multi-cloud-control-plane'),
    }),
  ],
});
