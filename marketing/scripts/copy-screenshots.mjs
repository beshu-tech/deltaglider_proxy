#!/usr/bin/env node
// Copy screenshots from the canonical home (../docs/screenshots/) into
// marketing/public/screenshots/ so Astro can serve them at /screenshots/*.
//
// Why: docs/screenshots/ is the single source of truth — also used by
// the product's embedded admin-UI docs viewer. Duplicating in
// marketing/public/ via .gitignore + this prebuild script keeps both
// in sync without check-in churn.
//
// Invoked automatically by `npm run build` via the prebuild hook.

import { cp, mkdir, readdir } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const src = join(root, '..', 'docs', 'screenshots');
const dst = join(root, 'public', 'screenshots');

await mkdir(dst, { recursive: true });

const entries = await readdir(src, { withFileTypes: true });
const files = entries.filter((e) => e.isFile() && /\.(jpe?g|png|webp|svg)$/i.test(e.name));

if (files.length === 0) {
    console.warn(`warn: no screenshots found in ${src}`);
} else {
    await Promise.all(
        files.map((f) =>
            cp(join(src, f.name), join(dst, f.name), { force: true }),
        ),
    );
    console.log(`copied ${files.length} screenshots from ../docs/screenshots/ → public/screenshots/`);
}
