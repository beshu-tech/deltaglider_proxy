/**
 * UX audit P2.5: an accessibility gate. axe-core (WCAG 2.0/2.1/2.2 A and AA,
 * incl. `target-size`, the 24×24 px rule) runs on every admin page and on the
 * object browser, in the light and the dark theme. A serious or critical
 * violation fails the spec.
 *
 * The admin pages come from the sidebar links themselves, so a new page is
 * checked without an edit here.
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

test.skip(!process.env.QA_REGRESSION, 'QA pass: run with `npm run e2e:qa`');
test.describe.configure({ timeout: 300_000 });

/** One line per serious/critical violation: rule, impact, first targets. */
async function violations(page: Page, where: string): Promise<string[]> {
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

async function setTheme(page: Page, theme: 'light' | 'dark') {
  await page.evaluate((t) => localStorage.setItem('dg-theme', t), theme);
}

/** Wait until the page settles: no skeleton, no spinner. */
async function settle(page: Page) {
  await page.waitForLoadState('networkidle').catch(() => {});
  await expect(page.locator('.ant-skeleton-active, .ant-spin-spinning')).toHaveCount(0, { timeout: 15_000 }).catch(() => {});
}

test('axe: no serious or critical violation on any page, both themes', async ({ page }) => {
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

  await page.setViewportSize({ width: 1440, height: 900 });
  await signIn(page);
  await page.goto('/_/admin/dashboard');
  const nav = page.getByRole('navigation', { name: 'Admin navigation' });
  await expect(nav).toBeVisible({ timeout: 30_000 });
  const adminPaths = await nav.locator('a[href]').evaluateAll((as) => as.map((a) => a.getAttribute('href') as string));
  expect(adminPaths.length).toBeGreaterThan(10);
  const paths = [`/_/browse/${BUCKET}/`, `/_/browse/${BUCKET}/builds/`, '/_/upload', ...adminPaths];

  const found: string[] = [];
  for (const theme of ['light', 'dark'] as const) {
    await setTheme(page, theme);
    for (const path of paths) {
      await page.goto(path);
      await settle(page);
      found.push(...(await violations(page, `${theme} ${path}`)));
    }
  }
  expect(found, found.join('\n')).toEqual([]);
});
