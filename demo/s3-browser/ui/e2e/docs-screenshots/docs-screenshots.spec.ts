/**
 * Captures every docs screenshot (shots/*.ts) in the light and the dark
 * theme. Run it through `scripts/docs-screenshots.sh`, which boots MinIO and
 * a release proxy, runs this spec, converts the PNGs to WebP and checks them.
 * Without DOCS_SCREENSHOTS=1 the spec skips itself, so `e2e-smoke` never runs it.
 *
 *   DOCS_SHOT_DIR   where the PNGs go (required)
 *   DOCS_ONLY       comma-separated shot ids: capture only these
 *   DOCS_CRATE_VERSION  the crate version; a shot whose page shows it fails
 */
import { test, expect, type Browser, type BrowserContext, type Page, type Request } from '@playwright/test';
import { mkdirSync } from 'node:fs';
import { clearAnnotations, drawAnnotations, markBox } from './annotate';
import { resolve, type Shot } from './shot';
import { SHOTS } from './shots';
import { BASE, seed, ADMIN_PASSWORD } from './seed';

test.skip(!process.env.DOCS_SCREENSHOTS, 'docs screenshots: run scripts/docs-screenshots.sh');
// Not serial: one failing shot must not hide the others. With one worker,
// beforeAll runs again after a failure; the seed skips a seeded proxy.
test.describe.configure({ timeout: 120_000 });
test.use({ actionTimeout: 15_000 });

const OUT = process.env.DOCS_SHOT_DIR ?? '';
const ONLY = new Set((process.env.DOCS_ONLY ?? '').split(',').filter(Boolean));
const VERSION = process.env.DOCS_CRATE_VERSION ?? '';
const THEMES = ['light', 'dark'] as const;
/** One "now" for the whole run (set by the script before the seed): relative times agree across shots, themes and worker restarts. */
const NOW = process.env.DOCS_NOW ? new Date(process.env.DOCS_NOW) : new Date();

type Storage = Awaited<ReturnType<BrowserContext['storageState']>>;
let adminState: Storage;

test.beforeAll(async ({ browser }) => {
  expect(OUT, 'DOCS_SHOT_DIR').not.toBe('');
  mkdirSync(OUT, { recursive: true });
  const ids = SHOTS.map((s) => s.id);
  expect(ids.filter((id, i) => ids.indexOf(id) !== i), 'duplicate shot ids').toEqual([]);
  for (const id of ids) expect(id, 'shot id: lower case and hyphens').toMatch(/^[a-z0-9]+(-[a-z0-9]+)*$/);
  // The alt text is the caption in both viewers (scripts/check-docs-images.sh has the same rule).
  for (const s of SHOTS) expect(s.alt.split(/\s+/).length >= 5 && s.alt.endsWith('.'), `${s.id}: alt is a full sentence`).toBe(true);
  for (const id of ONLY) expect(ids, `DOCS_ONLY names an unknown shot`).toContain(id);
  if (!process.env.DOCS_SKIP_SEED) await seed();
  adminState = await signIn(browser);
});

async function signIn(browser: Browser): Promise<Storage> {
  const ctx = await browser.newContext({ baseURL: BASE });
  const r = await ctx.request.post('/_/api/admin/login', {
    data: { password: ADMIN_PASSWORD },
    headers: { Origin: BASE },
  });
  expect(r.ok(), 'admin sign-in').toBe(true);
  const state = await ctx.storageState();
  await ctx.close();
  return state;
}

/**
 * Paint noise that must not reach an image: the caret, the scrollbars, the
 * message toasts. Motion is NOT turned off here: AntD waits for the end of
 * its open animations, so a dropdown would stay hidden. The capture itself
 * finishes running animations (`animations: 'disabled'`).
 */
const SHOT_CSS = `
  *, *::before, *::after { caret-color: transparent !important; }
  ::-webkit-scrollbar { display: none !important; }
  .ant-message, .ant-notification { display: none !important; }
`;

async function capture(browser: Browser, shot: Shot, theme: (typeof THEMES)[number]): Promise<void> {
  const viewport = shot.viewport ?? { width: 1280, height: 800 };
  const ctx = await browser.newContext({
    baseURL: BASE,
    viewport,
    deviceScaleFactor: 2,
    colorScheme: theme,
    reducedMotion: 'reduce',
    storageState: shot.auth === 'none' ? undefined : adminState,
  });
  try {
    await ctx.addInitScript((t) => {
      try {
        localStorage.setItem('dg-theme', t);
      } catch {
        /* blocked storage: colorScheme still picks the theme */
      }
    }, theme);
    // The running version and build time never reach a screenshot: the
    // images are public (served at /_/screenshots without a session).
    await ctx.route('**/_/api/whoami', async (route) => {
      const res = await route.fetch();
      const body = await res.json().catch(() => null);
      if (body && typeof body === 'object') {
        delete body.version;
        delete body.build_time;
      }
      await route.fulfill({ response: res, json: body });
    });
    const page = await ctx.newPage();
    const quiet = trackRequests(page);
    await page.clock.setFixedTime(NOW);
    await page.goto(shot.route);
    await settle(page, quiet);
    await page.addStyleTag({ content: SHOT_CSS });
    if (shot.setup) {
      await shot.setup(page);
      await settle(page, quiet);
    }
    // An autosize textarea can measure itself before the layout settles, and
    // its height then differs from run to run: make it measure again.
    await page.evaluate(
      () =>
        new Promise<void>((done) => {
          window.dispatchEvent(new Event('resize'));
          requestAnimationFrame(() => requestAnimationFrame(() => done()));
        }),
    );
    let boxes;
    try {
      boxes = await drawAnnotations(page, shot.annotations ?? []);
    } catch (e) {
      // What the page looked like when a target was missing: <id>.<theme>.fail.png, never converted.
      await page.screenshot({ path: `${OUT}/../fail/${shot.id}.${theme}.fail.png`, fullPage: true }).catch(() => undefined);
      throw e;
    }
    let clip: { x: number; y: number; width: number; height: number } | undefined;
    if (shot.clip) {
      const b = await markBox(page, shot.clip);
      const p = shot.clipPadding ?? 16;
      const x = Math.max(0, b.x - p);
      const y = Math.max(0, b.y - p);
      clip = {
        x,
        y,
        width: Math.min(viewport.width, b.x + b.width + p) - x,
        height: Math.min(viewport.height, b.y + b.height + p) - y,
      };
      for (const bb of boxes) {
        const inside = bb.x >= clip.x && bb.y >= clip.y && bb.x + bb.width <= clip.x + clip.width && bb.y + bb.height <= clip.y + clip.height;
        expect(inside, `${shot.id}: an annotation target lies outside the clip`).toBe(true);
      }
    }
    if (VERSION) {
      const text = await page.evaluate(() => document.body.innerText);
      expect(text.includes(`v${VERSION}`) || new RegExp(`\\b${VERSION.replace(/\./g, '\\.')}\\b`).test(text), `${shot.id}: the page shows the crate version`).toBe(false);
    }
    const masks = [page.locator('[data-shot-mask]'), ...(shot.mask ?? []).map((m) => resolve(page, m))];
    await page.screenshot({
      path: `${OUT}/${shot.id}.${theme}.png`,
      clip,
      animations: 'disabled',
      caret: 'hide',
      scale: 'device',
      mask: masks,
      maskColor: theme === 'dark' ? '#1e293b' : '#e2e8f0',
    });
    await clearAnnotations(page);
  } finally {
    if (shot.teardown) await shot.teardown(ctx.request);
    await ctx.close();
  }
}

/**
 * Resolves when no request (other than a long-lived stream) has been in
 * flight for QUIET_MS. `networkidle` is not enough: a page that polls is
 * never idle, and a list that loads late (prefix suggestions) then misses
 * a capture on one run and not on the next.
 */
const QUIET_MS = 750;
function trackRequests(page: Page): () => Promise<void> {
  const inflight = new Set<Request>();
  let lastChange = Date.now();
  const isStream = (r: Request) => /\/stream(\?|$)/.test(r.url()) || r.resourceType() === 'eventsource';
  page.on('request', (r) => {
    if (isStream(r)) return;
    inflight.add(r);
    lastChange = Date.now();
  });
  const done = (r: Request) => {
    if (inflight.delete(r)) lastChange = Date.now();
  };
  page.on('requestfinished', done);
  page.on('requestfailed', done);
  return async () => {
    const end = Date.now() + 15_000;
    while (Date.now() < end) {
      if (inflight.size === 0 && Date.now() - lastChange >= QUIET_MS) return;
      await page.waitForTimeout(100);
    }
    throw new Error(`requests still in flight: ${[...inflight].map((r) => r.url()).join(', ')}`);
  };
}

/** Wait until the page stops loading: requests, fonts, spinners, skeletons. */
async function settle(page: Page, quiet: () => Promise<void>): Promise<void> {
  await page.waitForLoadState('load');
  await quiet();
  await page.evaluate(() => document.fonts.ready);
  await expect(page.locator('.ant-spin-spinning, .ant-skeleton-active')).toHaveCount(0, { timeout: 15_000 });
  await quiet();
}

for (const shot of SHOTS) {
  test(shot.id, async ({ browser }) => {
    test.skip(ONLY.size > 0 && !ONLY.has(shot.id), 'not in DOCS_ONLY');
    for (const theme of THEMES) await capture(browser, shot, theme);
  });
}
