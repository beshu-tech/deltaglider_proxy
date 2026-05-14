// @ts-check
import { defineConfig } from 'astro/config';

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
});
