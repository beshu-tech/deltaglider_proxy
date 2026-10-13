/**
 * QA nightly flow for a replication rule, the way an operator meets it:
 *
 * New job → Replication rule → fill the buckets → Review & apply → the row
 * shows the run as running, with its live progress → it settles with the
 * number of objects that it copied → the Verify tab audits the mirror and
 * says "Verified in sync".
 *
 * Only a real browser + real server + real timers show this: the rule row
 * reported zero progress until the run ended, and the Verify tab hid the
 * verdict. Run with the other QA specs: `npm run e2e:qa`
 * (scripts/qa-e2e.sh starts the proxy). Without QA_REGRESSION=1 the spec
 * skips itself.
 */
import { test, expect, type Page } from '@playwright/test';
import { S3Client, CreateBucketCommand, PutObjectCommand } from '@aws-sdk/client-s3';

const PASSWORD = 'testpass';
const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:18600';
const ACCESS_KEY = process.env.QA_ACCESS_KEY ?? 'qa-admin-key';
const SECRET_KEY = process.env.QA_SECRET_KEY ?? 'qa-admin-secret-0123456789';
const RUN = Date.now().toString(36);
const SRC = `qa-repl-src-${RUN}`;
const DST = `qa-repl-dst-${RUN}`;
// Enough data that the run lasts a few seconds on a local disk, so the row
// is seen while it runs.
const OBJECTS = 300;
const SIZE = 512 * 1024;

test.skip(!process.env.QA_REGRESSION, 'QA pass: run with `npm run e2e:qa`');
test.describe.configure({ timeout: 120_000 });

function s3(): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
  });
}

function collectErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
  page.on('console', (m) => {
    if (m.type() === 'error' && !m.text().startsWith('Failed to load resource')) errors.push(m.text());
  });
  return errors;
}

test('replication: create a rule, watch it run, verify the mirror', async ({ page }) => {
  const client = s3();
  await client.send(new CreateBucketCommand({ Bucket: SRC }));
  await client.send(new CreateBucketCommand({ Bucket: DST }));
  const body = new Uint8Array(SIZE);
  for (let start = 0; start < OBJECTS; start += 20) {
    await Promise.all(
      Array.from({ length: Math.min(20, OBJECTS - start) }, (_, j) => {
        const i = start + j;
        body[0] = i % 256;
        return client.send(
          new PutObjectCommand({ Bucket: SRC, Key: `photos/img-${String(i).padStart(4, '0')}.jpg`, Body: body.slice() }),
        );
      }),
    );
  }

  const errors = collectErrors(page);
  await page.setViewportSize({ width: 1400, height: 900 });
  await page.goto('/_/browse');
  await page.getByPlaceholder('Admin password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });
  await page.goto('/_/admin/jobs');
  await expect(page.getByRole('heading', { level: 2, name: /^Jobs/ })).toBeVisible({ timeout: 30_000 });

  // New job → Replication rule: a draft opens in the drawer.
  await page.getByRole('button', { name: 'New job' }).click();
  await page.getByText('Replication rule — continuous copy').click();
  const drawer = page.getByRole('dialog');
  await drawer.getByPlaceholder('prod-artifacts').fill(SRC);
  await drawer.getByPlaceholder('backup-artifacts').fill(DST);
  await drawer.getByPlaceholder('backup-artifacts').press('Tab');

  // Review & apply → Apply and Persist.
  await page.getByRole('button', { name: /Review & apply/ }).first().click();
  await page.getByRole('button', { name: 'Apply and persist changes' }).click();

  const row = page.getByRole('row').filter({ hasText: SRC });
  await expect(row).toHaveCount(1, { timeout: 15_000 });
  // The scheduler starts the rule (it is due at once) within one tick (30 s).
  // While it runs, the row shows the run and its live counts.
  await expect(row).toContainText(/running/i, { timeout: 45_000 });
  await expect(row).toContainText(/\d[\d,]* copied/, { timeout: 15_000 });
  // It settles, and its run says how many objects it copied.
  await expect(row).toContainText(/succeeded/i, { timeout: 60_000 });
  // The drawer of the new rule stays open through the apply.
  await expect(drawer).toBeVisible();
  await drawer.getByRole('tab', { name: /Runs/ }).click();
  await expect(drawer.getByText(new RegExp(`${OBJECTS.toLocaleString('en-US')} copied`)).first()).toBeVisible({
    timeout: 15_000,
  });

  // Verify: the audit says the mirror is in sync.
  await drawer.getByRole('tab', { name: /Verify/ }).click();
  await drawer.getByRole('button', { name: 'Run audit' }).click();
  await expect(drawer.getByText('Verified in sync')).toBeVisible({ timeout: 60_000 });
  await expect(
    drawer.getByText(`All ${OBJECTS.toLocaleString('en-US')} objects are present and verified identical.`),
  ).toBeVisible();

  expect(errors).toEqual([]);
});
