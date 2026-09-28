#!/usr/bin/env node
// Post-processing for scripts/docs-screenshots.sh: PNG -> WebP, the size
// budget, and the comparison with the committed files.
//
//   node scripts/docs-screenshots-finish.mjs --png <dir> --webp <dir> --committed <docs/screenshots> [--update]
//
// Budget: 150 KB per file, FOLDER_BUDGET for the folder. The binary embeds
// docs/screenshots/ (rust-embed), so every byte here is in every download.
// WebP starts at quality 65 (text at device scale 2 stays sharp; 80 cost a
// quarter more bytes for no visible gain) and steps down to 50 for a busy
// shot; a file that is still over 150 KB fails the run.
//
// Compare: a pixel differs when a channel moves by more than 26 (about 0.1
// of the range); a shot is stale when more than 0.5 percent of its pixels
// differ, or its size changed. Without --update a stale or new shot fails.
import { copyFileSync, readdirSync, statSync, existsSync } from 'node:fs';
import { basename, join } from 'node:path';
import sharp from 'sharp';

const arg = (name) => {
  const i = process.argv.indexOf(name);
  return i < 0 ? undefined : process.argv[i + 1];
};
const PNG = arg('--png');
const WEBP = arg('--webp');
const COMMITTED = arg('--committed');
const UPDATE = process.argv.includes('--update');
const FILE_BUDGET = 150 * 1024;
// 12 MB: the 90 shots of the docs take 11.3 MB at quality 65 (September
// 2026), with room for a few more. A new shot that does not fit replaces or
// crops an old one; raising the budget grows every binary download.
const FOLDER_BUDGET = 12 * 1024 * 1024;
const CHANNEL_DELTA = 26;
const STALE_FRACTION = 0.005;

async function toWebp(src, dst) {
  for (const quality of [65, 60, 55, 50]) {
    const info = await sharp(src).webp({ quality, effort: 6 }).toFile(dst);
    if (info.size <= FILE_BUDGET) return { size: info.size, quality };
  }
  return { size: statSync(dst).size, quality: 50 };
}

async function diffFraction(a, b) {
  const [ra, rb] = await Promise.all(
    [a, b].map((f) => sharp(f).removeAlpha().raw().toBuffer({ resolveWithObject: true })),
  );
  if (ra.info.width !== rb.info.width || ra.info.height !== rb.info.height) return 1;
  const pa = ra.data;
  const pb = rb.data;
  let bad = 0;
  for (let i = 0; i < pa.length; i += 3) {
    if (
      Math.abs(pa[i] - pb[i]) > CHANNEL_DELTA ||
      Math.abs(pa[i + 1] - pb[i + 1]) > CHANNEL_DELTA ||
      Math.abs(pa[i + 2] - pb[i + 2]) > CHANNEL_DELTA
    ) {
      bad++;
    }
  }
  return bad / (pa.length / 3);
}

const pngs = readdirSync(PNG).filter((f) => f.endsWith('.png')).sort();
if (pngs.length === 0) {
  console.error('finish: no PNG captured');
  process.exit(1);
}
let failed = false;
for (const f of pngs) {
  const name = basename(f, '.png') + '.webp';
  const dst = join(WEBP, name);
  const { size, quality } = await toWebp(join(PNG, f), dst);
  const committed = join(COMMITTED, name);
  let state = 'new';
  if (existsSync(committed)) {
    const frac = await diffFraction(dst, committed);
    state = frac > STALE_FRACTION ? `CHANGED (${(frac * 100).toFixed(2)}% of pixels)` : `same (${(frac * 100).toFixed(3)}%)`;
  }
  const over = size > FILE_BUDGET;
  console.log(`${name.padEnd(44)} ${String(Math.round(size / 1024)).padStart(4)} KB q${quality}  ${state}${over ? '  OVER BUDGET' : ''}`);
  if (over) failed = true;
  if (UPDATE && !over) copyFileSync(dst, committed);
  else if (!UPDATE && !state.startsWith('same')) failed = true;
}
const total = readdirSync(COMMITTED).reduce((n, f) => n + statSync(join(COMMITTED, f)).size, 0);
console.log(`docs/screenshots total: ${(total / 1024 / 1024).toFixed(1)} MB (budget ${FOLDER_BUDGET / 1024 / 1024} MB)`);
if (total > FOLDER_BUDGET) failed = true;
if (failed) {
  console.error(UPDATE ? 'finish: a file is over budget' : 'finish: shots differ from docs/screenshots/ (run with --update to refresh)');
  process.exit(1);
}
