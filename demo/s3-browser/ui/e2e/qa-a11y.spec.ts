/**
 * UX audit P2.5: an accessibility gate. axe-core (WCAG 2.0/2.1/2.2 A and AA,
 * incl. `target-size`, the 24×24 px rule) runs on every admin page and on the
 * object browser, in the light and the dark theme. A serious or critical
 * violation fails the spec. The sign-in page is checked before sign-in, every
 * style is checked (Instrument and Editorial), and every popup a page offers
 * (`aria-haspopup` triggers: account menu, row and bucket actions) is opened
 * and checked too: a closed menu is invisible to axe.
 *
 * The admin pages come from the sidebar links themselves, so a new page is
 * checked without an edit here.
 *
 * One test per theme (style x mode), so each has its own time limit. The
 * whole sweep in ONE test took 4.7 min against a 5 min limit; past it the
 * nightly (#153) failed with `Test timeout` and `RangeError: Invalid string
 * length` (the report builder choking on the interrupted call), naming no
 * page. A step that outlasts STEP_LIMIT_MS now fails with its own name.
 *
 * Run with the other QA specs: `npm run e2e:qa` (scripts/qa-e2e.sh starts
 * the proxy). Without QA_REGRESSION=1 the spec skips itself.
 */
import AxeBuilder from '@axe-core/playwright';
import { test, expect, type Page } from '@playwright/test';
import { S3Client, CreateBucketCommand, PutObjectCommand } from '@aws-sdk/client-s3';

const PASSWORD = process.env.QA_PASSWORD ?? 'testpass';
const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:18600';
const ACCESS_KEY = process.env.QA_ACCESS_KEY ?? 'qa-admin-key';
const SECRET_KEY = process.env.QA_SECRET_KEY ?? 'qa-admin-secret-0123456789';
const BUCKET = `qa-a11y-${Date.now().toString(36)}`;
const TAGS = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'];

/** One page (settle + scan + its popups) must end within this. */
const STEP_LIMIT_MS = 60_000;
/** One theme: every page, light or dark, one style. */
const THEME_LIMIT_MS = 240_000;

test.skip(!process.env.QA_REGRESSION, 'QA pass: run with `npm run e2e:qa`');
test.describe.configure({ timeout: THEME_LIMIT_MS });

/** Fail with `where` when `work` does not end within `ms`. */
async function within<T>(where: string, ms: number, work: () => Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const limit = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${where}: not finished within ${ms / 1000} s`)), ms);
  });
  try {
    return await Promise.race([work(), limit]);
  } finally {
    clearTimeout(timer);
  }
}

/** One line per serious/critical violation: rule, impact, first targets. */
async function violations(page: Page, where: string): Promise<string[]> {
  // A fading popup or a fade-in card is measured mid-opacity: low contrast
  // and stale listboxes that no user sees. Scan the settled page. rc-motion
  // sets its `*-enter`/`*-appear` class a frame after the click, so an empty
  // getAnimations() alone can pass before the fade starts: wait for both.
  // An infinite animation (a spinner, a pulse) never ends and is ignored by
  // name; a finite one that outlasts the timeout FAILS the spec: scanning an
  // unsettled page would hide or invent violations.
  await page
    .waitForFunction(
      () => {
        const motion = /\bant-[\w-]+-(enter|appear|leave)\b/;
        const moving = [...document.querySelectorAll('[class*="ant-"]')].some((el) =>
          motion.test(typeof el.className === 'string' ? el.className : ''),
        );
        const finiteRunning = document
          .getAnimations()
          .some((a) => a.playState === 'running' && a.effect?.getComputedTiming().iterations !== Infinity);
        return !moving && !finiteRunning;
      },
      null,
      { timeout: 10_000, polling: 'raf' },
    )
    .catch((e: unknown) => {
      throw new Error(`${where}: the page did not settle within 10 s (a finite animation or an ant-* motion class is still running): ${String(e)}`);
    });
  const result = await new AxeBuilder({ page })
    .withTags(TAGS)
    // Monaco draws its own editor surface; its internals are not ours to fix.
    .exclude('.monaco-editor')
    .analyze();
  return result.violations
    .filter((v) => v.impact === 'serious' || v.impact === 'critical')
    .map((v) => {
      const targets = v.nodes.slice(0, 3).map((n) => n.target.join(' ')).join(' | ');
      return `${where}: ${v.id} (${v.impact}, ${v.nodes.length}x) ${targets}`;
    });
}

async function signIn(page: Page) {
  await page.goto('/_/browse');
  await page.getByLabel('Admin password', { exact: true }).fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });
}

const STYLES = ['instrument', 'editorial'] as const;
const MODES = ['light', 'dark'] as const;

async function setTheme(page: Page, style: (typeof STYLES)[number], mode: (typeof MODES)[number]) {
  await page.evaluate(([s, m]) => {
    localStorage.setItem('dg-theme-style', s);
    localStorage.setItem('dg-theme', m);
  }, [style, mode]);
}

/** Open each popup the page offers, check it, close it. */
async function popupViolations(page: Page, where: string): Promise<string[]> {
  const found: string[] = [];
  const triggers = page.locator('[aria-haspopup]:visible');
  const count = await triggers.count();
  for (let i = 0; i < count; i++) {
    const trigger = triggers.nth(i);
    if (await trigger.isDisabled()) continue;
    const name = (await trigger.getAttribute('aria-label')) ?? (await trigger.innerText()).trim();
    await trigger.click();
    await expect(trigger).toHaveAttribute('aria-expanded', 'true');
    found.push(...(await violations(page, `${where} [open: ${name}]`)));
    await page.keyboard.press('Escape');
    await expect(trigger).toHaveAttribute('aria-expanded', 'false');
  }
  return found;
}

/** Wait until the page settles: no skeleton, no spinner. */
async function settle(page: Page) {
  await page.waitForLoadState('networkidle').catch(() => {});
  await expect(page.locator('.ant-skeleton-active, .ant-spin-spinning')).toHaveCount(0, { timeout: 15_000 }).catch(() => {});
}

test.beforeAll(async () => {
  const s3 = new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
  });
  await s3.send(new CreateBucketCommand({ Bucket: BUCKET }));
  for (const key of ['builds/app-v1.zip', 'builds/app-v2.zip', 'notes.txt']) {
    await s3.send(new PutObjectCommand({ Bucket: BUCKET, Key: key, Body: `${key}\n`.repeat(64) }));
  }
});

for (const style of STYLES) {
  for (const mode of MODES) {
    test(`axe: no serious or critical violation on any page, ${style}/${mode}`, async ({ page }) => {
      await page.setViewportSize({ width: 1440, height: 900 });
      const found: string[] = [];
      await page.goto('/_/browse');
      await setTheme(page, style, mode);
      await page.reload();
      await expect(page.getByLabel('Admin password', { exact: true })).toBeVisible({ timeout: 30_000 });
      found.push(...(await within(`${style}/${mode} sign-in`, STEP_LIMIT_MS, () => violations(page, `${style}/${mode} sign-in`))));

      await signIn(page);
      await page.goto('/_/admin/dashboard');
      const nav = page.getByRole('navigation', { name: 'Admin navigation' });
      await expect(nav).toBeVisible({ timeout: 30_000 });
      const adminPaths = await nav.locator('a[href]').evaluateAll((as) => as.map((a) => a.getAttribute('href') as string));
      expect(adminPaths.length).toBeGreaterThan(10);
      const paths = [`/_/browse/${BUCKET}/`, `/_/browse/${BUCKET}/builds/`, '/_/upload', ...adminPaths];

      for (const path of paths) {
        const where = `${style}/${mode} ${path}`;
        const started = Date.now();
        await within(where, STEP_LIMIT_MS, async () => {
          await page.goto(path);
          await settle(page);
          found.push(...(await violations(page, where)));
          found.push(...(await popupViolations(page, where)));
        });
        console.log(`a11y ${where}: ${((Date.now() - started) / 1000).toFixed(1)} s`);
      }
      expect(found, found.join('\n')).toEqual([]);
    });
  }
}
