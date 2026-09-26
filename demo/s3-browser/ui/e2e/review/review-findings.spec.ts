/**
 * Reproductions of the browser-review findings (2026-09). NOT part of the CI
 * merge gate: without REVIEW=1 every test skips, so `playwright test e2e/`
 * in scripts/e2e-smoke.sh does not run them.
 *
 * Each test states the EXPECTED behaviour, so it FAILS while the finding is
 * open and passes once it is fixed.
 *
 * Run against a proxy in IAM mode, signed in with an IAM admin key pair:
 *
 *   REVIEW=1 REVIEW_ACCESS_KEY=... REVIEW_SECRET_KEY=... \
 *   PLAYWRIGHT_BASE_URL=http://127.0.0.1:9200 \
 *   npx playwright test e2e/review/
 *
 * The tests seed their own bucket (unique name per run) and delete the users
 * they create. The lifecycle test aborts the run-now request, so it never
 * deletes objects.
 */
import { test, expect, type Page } from '@playwright/test';
import { S3Client, CreateBucketCommand, PutObjectCommand, GetObjectCommand } from '@aws-sdk/client-s3';
import { randomBytes } from 'node:crypto';

const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:9200';
const ACCESS_KEY = process.env.REVIEW_ACCESS_KEY ?? '';
const SECRET_KEY = process.env.REVIEW_SECRET_KEY ?? '';
const RUN = Date.now().toString(36);
const BUCKET = `review-${RUN}`;

test.skip(!process.env.REVIEW, 'Review reproductions: run with REVIEW=1');
// Independent tests: each one reports its own finding.
test.describe.configure({ timeout: 120_000 });
test.use({ viewport: { width: 1440, height: 900 }, actionTimeout: 15_000 });

function s3(): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
  });
}

async function signIn(page: Page) {
  await page.goto('/_/admin');
  await page.getByPlaceholder('Access Key ID').fill(ACCESS_KEY);
  await page.getByPlaceholder('Secret Access Key').fill(SECRET_KEY);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByText('ADMIN SETTINGS', { exact: false })).toBeVisible({ timeout: 30_000 });
}

/** Parses "13.1 MB" / "256.0 MB" / "0 B" (formatBytes output, 1024 base). */
function parseBytes(s: string): number {
  const m = s.trim().match(/^([\d.,]+)\s*(B|KB|MB|GB|TB)$/);
  if (!m) throw new Error(`cannot parse size: ${s}`);
  const n = parseFloat(m[1].replace(/,/g, ''));
  const pow = ['B', 'KB', 'MB', 'GB', 'TB'].indexOf(m[2]);
  return n * 1024 ** pow;
}

test.beforeAll(async () => {
  expect(ACCESS_KEY, 'set REVIEW_ACCESS_KEY').not.toBe('');
  const c = s3();
  // beforeAll runs again in a fresh worker after a failed test.
  await c.send(new CreateBucketCommand({ Bucket: BUCKET })).catch(() => undefined);
  await c.send(new PutObjectCommand({ Bucket: BUCKET, Key: 'docs/README.txt', Body: 'hello from the review spec\n' }));
  // Two near-identical archives: the second is a delta, and reading it puts
  // the folder's reference baseline into the reference cache.
  const payload = randomBytes(2 * 1024 * 1024);
  await c.send(new PutObjectCommand({ Bucket: BUCKET, Key: 'builds/app-1.0.0.tar', Body: payload }));
  await c.send(new PutObjectCommand({ Bucket: BUCKET, Key: 'builds/app-1.0.1.tar', Body: Buffer.concat([payload, Buffer.from('v1.0.1')]) }));
  const got = await c.send(new GetObjectCommand({ Bucket: BUCKET, Key: 'builds/app-1.0.1.tar' }));
  await got.Body!.transformToByteArray();
});

test('creating a user without a secret shows the generated secret once', async ({ page }) => {
  await signIn(page);
  await page.goto('/_/admin/access/users');
  await page.getByRole('button', { name: /New/ }).click();
  const name = `review-user-${RUN}`;
  await page.getByPlaceholder('e.g. ci-bot').fill(name);
  const created = page.waitForResponse((r) => r.url().endsWith('/_/api/admin/users') && r.request().method() === 'POST');
  await page.locator('button:has-text("Create User")').click();
  const body = (await (await created).json()) as { id: number; secret_access_key: string };
  try {
    // The API returns the secret exactly once; the UI must show it, or the
    // new user has credentials that nobody knows.
    await expect(page.getByText(body.secret_access_key, { exact: false })).toBeVisible({ timeout: 5_000 });
  } finally {
    await page.request.delete(`/_/api/admin/users/${body.id}`);
  }
});

test('duplicating a user shows the fresh secret once', async ({ page }) => {
  await signIn(page);
  const src = await page.request.post('/_/api/admin/users', {
    data: { name: `review-src-${RUN}`, enabled: true, permissions: [{ effect: 'Allow', actions: ['read'], resources: [`${BUCKET}/*`] }] },
  });
  const srcUser = (await src.json()) as { id: number };
  await page.goto(`/_/admin/access/users?user=${srcUser.id}`);
  const cloned = page.waitForResponse((r) => /\/users\/\d+\/clone$/.test(r.url()));
  await page
    .locator('div')
    .filter({ hasText: new RegExp(`^review-src-${RUN}`) })
    .locator('button[title="Duplicate user with fresh credentials"]')
    .last()
    .click();
  const clone = (await (await cloned).json()) as { id: number; secret_access_key: string };
  try {
    await expect(page.getByText(clone.secret_access_key, { exact: false })).toBeVisible({ timeout: 5_000 });
  } finally {
    await page.request.delete(`/_/api/admin/users/${clone.id}`);
    await page.request.delete(`/_/api/admin/users/${srcUser.id}`);
  }
});

test('command palette "Show YAML" opens the export dialog', async ({ page }) => {
  await signIn(page);
  await page.goto('/_/admin/dashboard');
  await expect(page.getByText('Health, metrics, and savings at a glance.')).toBeVisible();
  await page.keyboard.press('Control+k');
  const search = page.getByPlaceholder('Type to filter pages or actions...');
  await expect(search).toBeFocused();
  await search.fill('show yaml');
  await expect(page.getByText('View current config as canonical YAML (secrets redacted)')).toBeVisible();
  await page.keyboard.press('Enter');
  await expect(page.getByText('Export configuration as YAML')).toBeVisible({ timeout: 5_000 });
});

test('double-clicking a text file opens the preview', async ({ page }) => {
  await signIn(page);
  await page.goto(`/_/browse/${BUCKET}/docs/`);
  const cell = page.getByText('README.txt', { exact: true });
  await expect(cell).toBeVisible({ timeout: 15_000 });
  const box = (await cell.boundingBox())!;
  // A real double-click: the first click opens the inspector drawer, whose
  // mask then receives the second click.
  await page.mouse.dblclick(box.x + 5, box.y + 5);
  await expect(page.locator('.ant-modal:visible').getByText('hello from the review spec')).toBeVisible({ timeout: 5_000 });
});

test('a passthrough file on a compressing bucket does not claim compression is off', async ({ page }) => {
  await signIn(page);
  await page.goto(`/_/browse/${BUCKET}/docs/`);
  await page.getByText('README.txt', { exact: true }).click();
  await expect(page.locator('.ant-drawer-open')).toBeVisible();
  await expect(page.locator('.ant-drawer-open').getByText('Compression disabled for this bucket')).toHaveCount(0);
});

test('dashboard "avg per entry" divides the used cache bytes, not the cache size', async ({ page }) => {
  await signIn(page);
  await page.goto('/_/admin/dashboard');
  const util = page.getByText(/^[\d.,]+ (B|KB|MB|GB) of [\d.,]+ (B|KB|MB|GB)$/).first();
  await expect(util).toBeVisible({ timeout: 15_000 });
  const [used] = (await util.innerText()).split(' of ').map(parseBytes);
  const avgText = await page.getByText(/avg per entry$/).first().innerText();
  const avg = parseBytes(avgText.replace(/\s*avg per entry$/, ''));
  // The count sits right above the "Active reference baselines" caption.
  const caption = page.getByText('Active reference baselines');
  const statText = await caption.locator('xpath=..').innerText();
  const entries = parseInt(statText.replace(/,/g, '').match(/\d+/)?.[0] ?? '0', 10);
  test.skip(entries === 0, 'needs at least one cached reference baseline');
  expect(avg * entries).toBeLessThanOrEqual(used * 1.1 + 1024);
});

test('analytics hero never says more than 100% smaller', async ({ page }) => {
  await signIn(page);
  await page.goto('/_/admin/dashboard');
  await page.getByText('Analytics', { exact: true }).click();
  const label = page.getByText('smaller on disk');
  test.skip((await label.count()) === 0, 'hero uses the plain-percent layout for this data');
  await expect(label).toBeVisible();
  // Wait for the count-up animation to settle.
  await page.waitForTimeout(2_500);
  const hero = await page.locator('text=STORAGE SAVED').locator('xpath=..').innerText();
  const lead = parseInt(hero.replace(/,/g, '').match(/(\d+)\s*%/)![1], 10);
  expect(lead, `hero reads "${lead}% smaller on disk"`).toBeLessThanOrEqual(100);
});

test('lifecycle "Run now" asks for confirmation before deleting', async ({ page }) => {
  await signIn(page);
  await page.goto('/_/admin/jobs');
  await expect(page.getByText('Everything that runs in the background', { exact: false })).toBeVisible();
  const row = page.locator('tr, [role=row]').filter({ hasText: 'Lifecycle' }).filter({ has: page.locator('button:has-text("Run now")') }).last();
  const hasRule = await row.waitFor({ timeout: 10_000 }).then(() => true, () => false);
  test.skip(!hasRule, 'needs a lifecycle rule');
  let sent = false;
  // Never let the run start: a missing confirmation must not delete objects.
  await page.route('**/_/api/admin/jobs/lifecycle*/run-now', async (route) => {
    sent = true;
    await route.abort();
  });
  await row.locator('button:has-text("Run now")').click();
  await page.waitForTimeout(1_000);
  expect(sent, 'run-now request went out without a confirmation step').toBe(false);
  await expect(page.locator('.ant-modal:visible, .ant-popover:visible')).toHaveCount(1);
});
