#!/usr/bin/env node
import { cp, mkdir, readdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const src = join(root, '..', 'docs', 'screenshots');
const dst = join(root, 'public', 'screenshots');

await mkdir(dst, { recursive: true });

const entries = await readdir(src, { withFileTypes: true });
let copied = 0;
for (const entry of entries) {
  if (!entry.isFile()) continue;
  if (entry.name.startsWith('.')) continue;
  await cp(join(src, entry.name), join(dst, entry.name));
  copied += 1;
}

console.log(`copy-screenshots: ${copied} file(s) → public/screenshots/`);
