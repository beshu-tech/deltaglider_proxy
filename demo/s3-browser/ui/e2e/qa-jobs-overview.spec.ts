/**
 * QA nightly flow for the Jobs screen as a whole (not one job kind): what
 * only a real browser + real server + real timers show.
 *
 * - ⌘K / Ctrl+K → "Jobs" opens the screen.
 * - A job started in ONE tab shows up in a SECOND open tab without a reload
 *   (the idle poll), and settles there with its own words.
 * - At 390 px the rows and the drawer fit the screen, and the meter label
 *   is not cut.
 *
 * Run with the other QA specs: `npm run e2e:qa` (scripts/qa-e2e.sh starts
 * the proxy). Without QA_REGRESSION=1 the spec skips itself.
 */
import { test, expect, type Page } from '@playwright/test';
import { S3Client, CreateBucketCommand, PutObjectCommand } from '@aws-sdk/client-s3';

const PASSWORD = 'testpass';
const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:18600';
const ACCESS_KEY = process.env.QA_ACCESS_KEY ?? 'qa-admin-key';
const SECRET_KEY = process.env.QA_SECRET_KEY ?? 'qa-admin-secret-0123456789';
const BUCKET = `qa-jobs-ov-${Date.now().toString(36)}`;

test.skip(!process.env.QA_REGRESSION, 'QA pass: run with `npm run e2e:qa`');
test.describe.configure({ timeout: 90_000 });

function s3(): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
  });
}

/** Console errors and uncaught page errors; responses are not checked here. */
function collectErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
  page.on('console', (m) => {
    if (m.type() === 'error' && !m.text().startsWith('Failed to load resource')) errors.push(m.text());
  });
  return errors;
}

function jobRow(page: Page) {
  return page.getByRole('row').filter({ hasText: 'Backfill metadata' }).filter({ hasText: BUCKET });
}

test('jobs overview: palette, second tab sees a new job, 390 px layout', async ({ browser }) => {
  // Seed: a bucket with objects written through the proxy (already stamped).
  await s3().send(new CreateBucketCommand({ Bucket: BUCKET }));
  for (let i = 0; i < 20; i += 1) {
    await s3().send(new PutObjectCommand({ Bucket: BUCKET, Key: `app/build-${i}.txt`, Body: `build ${i}\n` }));
  }

  const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  const tabA = await ctx.newPage();
  const errorsA = collectErrors(tabA);

  // Sign in (bootstrap password), then ⌘K → Jobs.
  await tabA.goto('/_/browse');
  await tabA.getByPlaceholder('Admin password').fill(PASSWORD);
  await tabA.getByRole('button', { name: 'Sign in' }).click();
  await expect(tabA.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });
  await tabA.goto('/_/admin/dashboard');
  await expect(tabA.getByRole('heading', { level: 2, name: /^Dashboard/ })).toBeVisible({ timeout: 30_000 });
  await tabA.keyboard.press('ControlOrMeta+K');
  const palette = tabA.getByPlaceholder('Type to filter pages or actions...');
  await expect(palette).toBeVisible();
  await palette.fill('Jobs');
  await tabA.keyboard.press('Enter');
  await expect(tabA).toHaveURL(/\/_\/admin\/jobs$/);
  await expect(tabA.getByRole('heading', { level: 2, name: /^Jobs/ })).toBeVisible();

  // Tab B: the Jobs screen on a phone width, open before the job exists.
  const tabB = await ctx.newPage();
  const errorsB = collectErrors(tabB);
  await tabB.setViewportSize({ width: 390, height: 844 });
  await tabB.goto('/_/admin/jobs');
  await expect(tabB.getByText(/jobs? — select one|No jobs yet/).first()).toBeVisible({ timeout: 30_000 });
  await expect(jobRow(tabB)).toHaveCount(0);

  // Start a job from tab A (the admin API of the signed-in session).
  const started = await tabA.request.post('/_/api/admin/jobs/backfill-metadata', {
    data: { buckets: [BUCKET], refresh_last_modified: false },
  });
  expect(started.ok()).toBe(true);

  // Tab B sees it without a reload: the idle poll is 10 s.
  await expect(jobRow(tabB)).toHaveCount(1, { timeout: 15_000 });
  // It settles with backfill words: every object was written through the
  // proxy, so all of them already carry the metadata.
  await expect(jobRow(tabB)).toContainText(/already stamped/, { timeout: 30_000 });
  await expect(jobRow(tabB)).not.toContainText(/in sync|straight copy/);

  // 390 px: nothing scrolls sideways, and the meter label is not cut.
  const overflow = await tabB.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
  const label = jobRow(tabB).locator('.dg-meter-label');
  const cut = await label.evaluate((el) => el.scrollWidth > el.clientWidth + 1);
  expect(cut).toBe(false);

  // The drawer fits the screen and shows what the job did.
  await jobRow(tabB).getByText(BUCKET).first().click();
  const drawer = tabB.getByRole('dialog');
  await expect(drawer.getByText('Last-Modified')).toBeVisible();
  // After the slide-in the drawer panel lies inside the page. Regression:
  // a 100vw panel next to a scrollbar lost its left 6 px off screen.
  await expect
    .poll(async () => {
      const box = await drawer.boundingBox();
      const page = await tabB.evaluate(() => document.documentElement.clientWidth);
      return box ? [Math.round(box.x), Math.round(box.x + box.width) <= page] : null;
    })
    .toEqual([0, true]);
  await expect(drawer.getByText('kept as it was')).toBeVisible();
  await expect(drawer.getByText('Objects', { exact: true })).toBeVisible();

  expect(errorsA).toEqual([]);
  expect(errorsB).toEqual([]);
  await ctx.close();
});
