/**
 * QA nightly flow for a replication rule, the way an operator meets it:
 *
 * Storage → Replication → New replication rule → fill the buckets → Review & apply → the row
 * carries the run it started and the number of objects that it copied → the
 * Verify tab audits the mirror and says "Verified in sync" → Clone opens a
 * draft that warns while it copies the same objects → "All runs →" opens the
 * Jobs page filtered to the rule, and its run drawer leads back to the rule.
 *
 * Only a real browser + real server + real timers show this: the rule row
 * reported zero progress until the run ended, and the Verify tab hid the
 * verdict. What the row looks like WHILE it runs (the meter, "running for
 * 3s") cannot be asserted here: the scheduler starts a new rule at its next
 * check (up to 30 s later) and the mirror of a local bucket finishes in under
 * a second, so no 2 s list poll lands inside the run — the row would only be
 * asserted by luck. The live row is covered by the pure view tests in
 * src/__tests__/jobsRunState.test.ts (lastRunCell, statusCellView).
 * Run with the other QA specs: `npm run e2e:qa`
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
// Enough data that the mirror is a real copy of 300 objects, and big enough
// that the Verify audit has something to hash.
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
  await page.goto('/_/admin/storage/replication');
  await expect(page.getByRole('heading', { level: 2, name: /^Replication/ })).toBeVisible({ timeout: 30_000 });

  // New replication rule: a draft opens in the drawer.
  await page.getByRole('button', { name: 'New replication rule' }).click();
  const drawer = page.getByRole('dialog');
  await drawer.getByPlaceholder('prod-artifacts').fill(SRC);
  await drawer.getByPlaceholder('backup-artifacts').fill(DST);
  await drawer.getByPlaceholder('backup-artifacts').press('Tab');

  // Review & apply → Apply and Persist.
  await page.getByRole('button', { name: /Review & apply/ }).first().click();
  await page.getByRole('button', { name: 'Apply and persist changes' }).click();

  const row = page.getByRole('row').filter({ hasText: SRC });
  await expect(row).toHaveCount(1, { timeout: 15_000 });
  // The scheduler starts the rule (it is due at once) at its next check, up
  // to 30 s later; its run settles with the number of objects that it copied.
  // No poll lands while it runs (see the header), so wait for the outcome.
  await expect(row).toContainText(/succeeded/i, { timeout: 60_000 });
  await expect(row).toContainText(/\d[\d,]* copied/, { timeout: 15_000 });
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

  // Clone: a draft "<name>-copy" opens, and it warns while it copies the
  // same objects as the rule it came from. Discard drops it.
  const ruleName = (await row.locator('[data-col="rule"] [title]').first().getAttribute('title')) ?? '';
  expect(ruleName).not.toBe('');
  await page.keyboard.press('Escape');
  await expect(drawer).toBeHidden();
  await row.getByRole('button', { name: `More actions for ${ruleName}` }).click();
  await page.getByRole('menuitem', { name: 'Clone' }).click();
  const clone = page.getByRole('dialog');
  await expect(clone).toContainText(`${ruleName}-copy`);
  await expect(clone.getByText(`Rule "${ruleName}" has the same source, destination and filters`)).toBeVisible();
  await clone.getByPlaceholder('mirror/releases/').fill('elsewhere/');
  await expect(clone.getByText(/has the same source, destination and filters/)).toBeHidden();
  // The close button, not Escape: Escape first closes the prefix suggestions.
  await clone.getByRole('button', { name: 'Close', exact: true }).click();
  await expect(clone).toBeHidden();
  await page.getByRole('button', { name: 'Discard' }).click();
  await expect(page.getByRole('row').filter({ hasText: `${ruleName}-copy` })).toHaveCount(0);
  await expect(page.getByRole('dialog')).toHaveCount(0);

  // All runs →: the Jobs page, filtered to the rule, lists its run.
  await row.getByRole('button', { name: `All runs of ${ruleName}` }).click();
  await expect(page).toHaveURL(/\/_\/admin\/jobs\?rule=replication/);
  const run = page.getByRole('row').filter({ hasText: ruleName });
  await expect(run.first()).toContainText(new RegExp(`${OBJECTS.toLocaleString('en-US')} copied`), { timeout: 15_000 });
  await run.first().click();
  const runDrawer = page.getByRole('dialog');
  await expect(runDrawer.getByText('Failures of this run')).toBeVisible();
  await runDrawer.getByRole('button', { name: 'Open rule' }).click();
  await expect(page).toHaveURL(/\/_\/admin\/storage\/replication\?job=replication/);

  expect(errors).toEqual([]);
});
