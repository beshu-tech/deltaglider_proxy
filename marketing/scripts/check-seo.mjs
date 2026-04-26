#!/usr/bin/env node
import { readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const distDir = join(dirname(here), 'dist');

const PAGES = [
  { path: 'index.html', label: '/' },
  { path: 'regulated/index.html', label: '/regulated/' },
  { path: 'artifact-storage/index.html', label: '/artifact-storage/' },
  { path: 'minio-migration/index.html', label: '/minio-migration/' },
  { path: 's3-saas-control-plane/index.html', label: '/s3-saas-control-plane/' },
  { path: 'about/index.html', label: '/about/' },
  { path: 'privacy/index.html', label: '/privacy/' },
  { path: 'terms/index.html', label: '/terms/' },
];

const REQUIRED = [
  { name: 'title', re: /<title[^>]*>[^<]+<\/title>/ },
  {
    name: 'description',
    re: /<meta[^>]*\sname=["']description["'][^>]*\scontent=["'][^"']+["']/,
  },
  { name: 'canonical', re: /<link[^>]*\srel=["']canonical["']/ },
  { name: 'og:title', re: /<meta[^>]*\sproperty=["']og:title["']/ },
  { name: 'og:description', re: /<meta[^>]*\sproperty=["']og:description["']/ },
  { name: 'og:image', re: /<meta[^>]*\sproperty=["']og:image["']/ },
  { name: 'twitter:card', re: /<meta[^>]*\sname=["']twitter:card["']/ },
  {
    name: 'JSON-LD',
    re: /<script[^>]*\stype=["']application\/ld\+json["']/,
  },
];

let failures = 0;
for (const page of PAGES) {
  const filePath = join(distDir, page.path);
  let html;
  try {
    html = await readFile(filePath, 'utf8');
  } catch (err) {
    console.error(`✗ ${page.label}: failed to read ${filePath}: ${err.message}`);
    failures += 1;
    continue;
  }

  const missing = [];
  for (const check of REQUIRED) {
    if (!check.re.test(html)) missing.push(check.name);
  }

  if (missing.length > 0) {
    console.error(`✗ ${page.label}: missing ${missing.join(', ')}`);
    failures += 1;
  } else {
    console.log(`✓ ${page.label}: SEO checks passed`);
  }

  const ssgCanary =
    />Smaller object storage|>Not S3 object versioning|>Use cheap storage without trusting|>Self-hosted S3 without losing|>Use cheaper S3 storage|>Business impact|>DeltaGlider Proxy is built by Beshu Tech|>Privacy Policy|>Terms of Service/;
  if (!ssgCanary.test(html)) {
    console.error(
      `✗ ${page.label}: page content not found in raw HTML — SSG may not have pre-rendered`,
    );
    failures += 1;
  }
}

if (failures > 0) {
  console.error(`\ncheck-seo: ${failures} failure(s)`);
  process.exit(1);
} else {
  console.log(`\ncheck-seo: all ${PAGES.length} page(s) passed`);
}
