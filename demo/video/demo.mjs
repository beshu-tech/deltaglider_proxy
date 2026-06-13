// DeltaGlider Proxy — 90-second product demo recorder.
//
// Drives the admin UI through five live actions and records a silent WebM:
//   1. Add an encrypted (AES-256-GCM) + compression-enabled storage backend
//   2. Create a bucket routed to that backend
//   3. Create an IAM user + group with permissions
//   4. Browse + upload an object
//   5. Open the object's metadata drawer (delta + encryption savings)
//
// Captions are NOT burned here — this produces the raw screen capture. The
// ffmpeg compose step (compose.sh) overlays captions from captions.ass.
//
// Run from demo/s3-browser/ui (so Playwright resolves):
//   node ../../video/demo.mjs <base-url> <out-dir>
// e.g. node ../../video/demo.mjs http://127.0.0.1:9220 /private/tmp/dgp-demo-video/video
//
// The recorded .webm lands in <out-dir>; its exact name is printed at the end.

import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
// Resolve Playwright from the UI package (this script lives outside it).
const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(resolve(here, '../s3-browser/ui') + '/');
const { chromium } = require('playwright');

const BASE = process.argv[2] || 'http://127.0.0.1:9220';
const DEMO_DIR = process.env.DGP_DEMO_DIR || '/private/tmp/dgp-demo-video';
const OUT = process.argv[3] || `${DEMO_DIR}/video`;
const PASSWORD = 'testpassword123';

// 4:3 capture. Small-ish viewport keeps UI elements large/legible.
const VIEW = { width: 1280, height: 960 };

// Pacing helpers — deliberate beats so the eye can follow each action.
const beat = (ms) => new Promise((r) => setTimeout(r, ms));

const browser = await chromium.launch({ args: ['--force-device-scale-factor=2'] });
const ctx = await browser.newContext({
  viewport: VIEW,
  deviceScaleFactor: 2,
  recordVideo: { dir: OUT, size: VIEW },
  // light theme is set via initScript below
});
await ctx.addInitScript(() => {
  try {
    localStorage.setItem('dg-theme', 'light');
    sessionStorage.removeItem('dgp-hero-animated');
  } catch {}
});

const page = await ctx.newPage();

// Smooth cursor + click ripple so the viewer's eye tracks the action.
// (Purely cosmetic overlay injected into the page.)
await page.addInitScript(() => {
  window.__dgCursor = () => {
    if (document.getElementById('dg-cursor')) return;
    const c = document.createElement('div');
    c.id = 'dg-cursor';
    c.style.cssText =
      'position:fixed;z-index:2147483647;width:22px;height:22px;margin:-11px 0 0 -11px;border-radius:50%;' +
      'background:rgba(16,185,129,0.35);border:2px solid rgba(16,185,129,0.9);pointer-events:none;' +
      'transition:left .25s cubic-bezier(.22,1,.36,1),top .25s cubic-bezier(.22,1,.36,1),transform .12s;left:50%;top:50%;';
    document.body.appendChild(c);
    document.addEventListener('mousemove', (e) => {
      c.style.left = e.clientX + 'px';
      c.style.top = e.clientY + 'px';
    });
    document.addEventListener('mousedown', () => (c.style.transform = 'scale(0.7)'));
    document.addEventListener('mouseup', () => (c.style.transform = 'scale(1)'));
  };
});

// Move the real Playwright mouse to an element's center so the overlay tracks it,
// then a small dwell before clicking — reads as intentional, not teleporting.
async function point(locator) {
  const box = await locator.boundingBox().catch(() => null);
  if (box) {
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 18 });
    await beat(280);
  }
}
async function clickSlow(locator, { optional = false } = {}) {
  const loc = locator.first();
  try {
    await loc.waitFor({ state: 'attached', timeout: optional ? 4000 : 15000 });
    await loc.scrollIntoViewIfNeeded({ timeout: 5000 });
    await loc.waitFor({ state: 'visible', timeout: 8000 });
  } catch (e) {
    if (optional) return false;
    throw e;
  }
  await point(loc);
  await loc.click({ timeout: 8000 });
  await beat(450);
  return true;
}
async function typeSlow(locator, text) {
  const loc = locator.first();
  await loc.waitFor({ state: 'attached', timeout: 15000 });
  await loc.scrollIntoViewIfNeeded({ timeout: 5000 }).catch(() => {});
  await loc.waitFor({ state: 'visible', timeout: 8000 });
  await point(loc);
  await loc.click();
  await loc.fill('');
  await loc.pressSequentially(text, { delay: 55 });
  await beat(350);
}

// Wait until no AntD modal is on screen (the ApplyDialog closes async after a
// successful PUT; its wrap intercepts pointer events until it's gone).
async function waitNoModal() {
  await page
    .waitForFunction(() => {
      const w = document.querySelector('.ant-modal-wrap');
      return !w || w.offsetParent === null || getComputedStyle(w).display === 'none';
    }, { timeout: 10000 })
    .catch(() => {});
  await beat(300);
}

// SPA navigation that survives the router's skipNext guard.
async function gotoAdmin(path) {
  await page.evaluate((p) => {
    window.history.pushState(null, '', p);
    window.dispatchEvent(new PopStateEvent('popstate'));
    window.dispatchEvent(new PopStateEvent('popstate'));
  }, path);
  await beat(900);
}

// ── boot: load, set cursor, log in ──────────────────────────────────────────
await page.goto(`${BASE}/_/`, { waitUntil: 'domcontentloaded' });
await page.evaluate(() => window.__dgCursor && window.__dgCursor());
await beat(700);

// Login screen (bootstrap password).
const pwd = page.locator('input[type="password"]').first();
if (await pwd.isVisible().catch(() => false)) {
  await typeSlow(pwd, PASSWORD);
  await clickSlow(page.getByRole('button', { name: /sign in/i }));
  await page.waitForURL(/\/browse\//, { timeout: 15000 }).catch(() => {});
  await beat(1000);
}
if (process.env.DG_DEBUG) console.log('DEBUG post-login path:', await page.evaluate(() => location.pathname));
await page.evaluate(() => window.__dgCursor && window.__dgCursor());

// Helper: open an AntD Select (by testid wrapper) and pick an option by text.
async function pickFromSelect(wrapperTestId, optionText) {
  const wrap = page.getByTestId(wrapperTestId);
  await clickSlow(wrap.locator('.ant-select'));
  await beat(350);
  // Options render in a portal at body level.
  await clickSlow(
    page.locator('.ant-select-dropdown:visible .ant-select-item-option').filter({ hasText: optionText }).first(),
  );
  await beat(500);
}

const tid = (id) => page.getByTestId(id);

try {
// ── STEP 1: encrypted + compressed backend ──────────────────────────────────
await gotoAdmin('/_/admin/storage/backends');
await beat(1200);

await clickSlow(tid('backends-add'));
await typeSlow(tid('backend-name'), 'hetzner-fsn1');
await beat(400);
await clickSlow(tid('backend-create'));
await beat(1500);

// Encryption: pick AES-256-GCM (proxy-side).
await pickFromSelect('encryption-mode-select', 'AES-256-GCM (proxy-side)');
await beat(700);
// Confirm the key was stored, then Apply.
await clickSlow(tid('encryption-key-stored'));
await beat(400);
await clickSlow(tid('encryption-apply'));
await beat(900);
// ApplyDialog confirm (encryption change persists).
await clickSlow(tid('apply-dialog-confirm'), { optional: true });
await beat(900);
// "Encrypt existing objects?" may appear (offers a background re-encrypt job).
// Briefly show it, then decline — the demo doesn't wait on a job.
await clickSlow(page.getByRole('button', { name: /Later/i }), { optional: true });
await waitNoModal();
await beat(1000);

// Compression default toggle (left ON for the demo).
await clickSlow(tid('delta-compression-switch'), { optional: true });
await beat(1300);

// ── STEP 2: bucket routed to the backend ────────────────────────────────────
await gotoAdmin('/_/admin/storage/buckets');
await beat(1200);
await clickSlow(tid('buckets-create'));
await beat(800);
await typeSlow(tid('bucket-name'), 'db-archive');
await beat(300);
// Backend selector inside the modal (only shown when >1 backend).
if (await tid('bucket-backend-select').count()) {
  await pickFromSelect('bucket-backend-select', 'hetzner-fsn1');
}
await beat(400);
// Modal's "Create" ok button.
await clickSlow(page.locator('.ant-modal-footer button.ant-btn-primary'), { optional: true });
await beat(1200);
// Review & apply (bucket routing persists).
await clickSlow(tid('apply-dialog-confirm'), { optional: true });
await waitNoModal();
await beat(1400);

// ── STEP 3: IAM user + group ─────────────────────────────────────────────────
await gotoAdmin('/_/admin/access/users');
await beat(1200);
await clickSlow(tid('md-new-users'), { optional: true });
await beat(700);
await typeSlow(tid('user-name'), 'backup-bot');
await beat(900);
await clickSlow(tid('user-save'), { optional: true });
await beat(1700);
// Dismiss any credentials banner/modal.
await clickSlow(page.getByRole('button', { name: /Done|Close|Got it|I.ve saved/i }).first(), { optional: true });
await beat(800);

// Create group.
await gotoAdmin('/_/admin/access/groups');
await beat(1000);
await clickSlow(tid('md-new-groups'), { optional: true });
await beat(700);
await typeSlow(tid('group-name'), 'Engineering');
await beat(700);
await clickSlow(tid('group-save'), { optional: true });
await beat(1500);

// ── STEP 4: browse + upload ──────────────────────────────────────────────────
// Back to the object browser on the seeded releases bucket.
await page.evaluate(() => {
  window.history.pushState(null, '', '/_/browse/releases/firmware/widget-3000/');
  window.dispatchEvent(new PopStateEvent('popstate'));
  window.dispatchEvent(new PopStateEvent('popstate'));
});
await beat(1800);

// ── STEP 5: object metadata drawer ──────────────────────────────────────────
// Click the newest versioned tarball row → inspector drawer.
await clickSlow(tid('object-row-fw-1.3.0.tar'), { optional: true });
await beat(1800);
// Make sure the savings region is in view, then hold for the closing beat.
await tid('inspector-savings').first().scrollIntoViewIfNeeded({ timeout: 4000 }).catch(() => {});
await beat(3000);
} catch (err) {
  console.error('STEP FAILED:', err && err.message ? err.message.split('\n')[0] : err);
  await page.screenshot({ path: OUT + '/../debug-fail.png' }).catch(() => {});
  console.error('DEBUG path:', await page.evaluate(() => location.pathname).catch(() => '?'));
}

// ── flush ────────────────────────────────────────────────────────────────────
await ctx.close();
const video = await page.video();
const path = video ? await video.path().catch(() => null) : null;
await browser.close();
console.log('VIDEO_PATH=' + (path || '(check ' + OUT + ')'));
