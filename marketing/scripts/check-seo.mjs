#!/usr/bin/env node
import { readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const distDir = join(dirname(here), 'dist');

const PAGES = [
  { path: 'index.html', label: '/' },
  { path: 'regulated/index.html', label: '/regulated/' },
  { path: 'versioning/index.html', label: '/versioning/' },
  { path: 'minio-migration/index.html', label: '/minio-migration/' },
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

  const ssgCanary = />Cloud storage up to|>Up to 95%|>Your data never|>If you liked MinIO|>Three different problems/;
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
