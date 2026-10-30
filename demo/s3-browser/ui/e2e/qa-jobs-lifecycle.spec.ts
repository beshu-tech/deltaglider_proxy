/**
 * QA nightly flow for a lifecycle rule, end to end in a real browser:
 *
 *   create a delete rule in the UI over objects with OLD modification times
 *   (planted on the filesystem backend, as a restore leaves them) → apply →
 *   the preview counts them → Run now → the confirmation names that count →
 *   the run deletes exactly that many → the result message and the Runs tab
 *   say "deleted".
 *
 * Run with the other QA specs: `npm run e2e:qa` (scripts/qa-e2e.sh starts the
 * proxy and exports QA_DATA_DIR, the data dir of its filesystem backend).
 * Without QA_REGRESSION=1 the spec skips itself.
 */
import { test, expect } from '@playwright/test';
import { S3Client, CreateBucketCommand, ListObjectsV2Command, PutObjectCommand } from '@aws-sdk/client-s3';
import * as fs from 'node:fs';
import * as path from 'node:path';

const PASSWORD = 'testpass';
const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:18600';
const ACCESS_KEY = process.env.QA_ACCESS_KEY ?? 'qa-admin-key';
const SECRET_KEY = process.env.QA_SECRET_KEY ?? 'qa-admin-secret-0123456789';
const DATA_DIR = process.env.QA_DATA_DIR ?? '';
const SUFFIX = Date.now().toString(36);
const BUCKET = `qa-lc-${SUFFIX}`;
const RULE = `qa-expire-${SUFFIX}`;
const OLD = 12;
const NEW = 3;

test.skip(!process.env.QA_REGRESSION, 'QA pass: run with `npm run e2e:qa`');
test.skip(!DATA_DIR, 'needs QA_DATA_DIR (scripts/qa-e2e.sh exports it)');
test.describe.configure({ timeout: 90_000 });

function s3(): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
  });
}

/** Write `key` straight to the backend's disk with a 2020 modification time:
 *  an object that the proxy did not write, which lifecycle ages by mtime. */
function plantOld(key: string) {
  const file = path.join(DATA_DIR, BUCKET, 'deltaspaces', key);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `restored ${key}\n`);
  const t = new Date('2020-01-01T00:00:00Z');
  fs.utimesSync(file, t, t);
}

async function keysUnder(prefix: string): Promise<number> {
  const out = await s3().send(new ListObjectsV2Command({ Bucket: BUCKET, Prefix: prefix }));
  return out.KeyCount ?? 0;
}

test('lifecycle: create a rule, preview count = confirmed count = deleted count', async ({ page }) => {
  await s3().send(new CreateBucketCommand({ Bucket: BUCKET }));
  for (let i = 0; i < OLD; i += 1) plantOld(`logs/old-${String(i).padStart(2, '0')}.log`);
  for (let i = 0; i < NEW; i += 1) {
    await s3().send(new PutObjectCommand({ Bucket: BUCKET, Key: `logs/new-${i}.log`, Body: `new ${i}\n` }));
  }
  expect(await keysUnder('logs/')).toBe(OLD + NEW);

  // Sign in, open Storage → Lifecycle.
  await page.goto('/_/browse');
  await page.getByLabel('Admin password', { exact: true }).fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });
  await page.goto('/_/admin/storage/lifecycle');
  await expect(page.getByRole('heading', { level: 2, name: /^Lifecycle/ })).toBeVisible({ timeout: 30_000 });

  // New lifecycle rule: name, enabled, scope; 30d is the default age.
  await page.getByRole('button', { name: 'New lifecycle rule' }).click();
  const drawer = page.getByRole('dialog').last();
  const name = drawer.getByRole('textbox').first();
  await name.fill(RULE);
  await drawer.getByPlaceholder('prod-artifacts').fill(BUCKET);
  await drawer.getByPlaceholder('builds/releases/').fill('logs/');
  await drawer.getByRole('switch').first().click();
  await page.keyboard.press('Escape');
  // While the drawer is open its footer carries the apply bar; the page bar
  // returns once the drawer has closed.
  await expect(drawer).toBeHidden();

  // The master switch, then apply. The review step names the first run.
  const master = page.getByRole('switch', { name: 'Run lifecycle rules on schedule' });
  if ((await master.getAttribute('aria-checked')) !== 'true') await master.click();
  await page.getByRole('button', { name: /Review & apply/ }).click();
  const review = page.getByRole('dialog').filter({ hasText: 'Lifecycle plan' });
  await expect(review.getByText(/New schedule: first scheduled run about 1h after you apply/)).toBeVisible();
  await review.getByRole('button', { name: 'Apply and persist changes' }).click();
  await expect(review).toBeHidden({ timeout: 15_000 });

  // Preview: OLD objects are due; the NEW ones are not old enough yet.
  const row = page.getByRole('row').filter({ hasText: RULE });
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.getByRole('button', { name: `Preview: ${RULE}` }).click();
  await expect(page.getByText(new RegExp(`${OLD} of ${OLD + NEW} scanned`))).toBeVisible({ timeout: 30_000 });
  await expect(page.getByText(`Of the other scanned objects, ${NEW} are not old enough yet.`)).toBeVisible();
  await page.keyboard.press('Escape');

  // Run now: the confirmation names the same count, from a fresh walk.
  await row.getByRole('button', { name: `Run now: ${RULE}` }).click();
  const confirm = page.getByRole('dialog').filter({ hasText: `Run lifecycle rule "${RULE}" now?` });
  const runButton = confirm.getByRole('button', { name: `Run: delete ${OLD} objects` });
  await expect(runButton).toBeEnabled({ timeout: 30_000 });
  await expect(confirm.getByText(/s ago\)\.$/)).toBeVisible();
  await runButton.click();

  // The result message names what the run did, and the bucket agrees.
  await expect(page.getByText(`Lifecycle rule "${RULE}" finished: deleted ${OLD}.`)).toBeVisible({ timeout: 30_000 });
  expect(await keysUnder('logs/old-')).toBe(0);
  expect(await keysUnder('logs/new-')).toBe(NEW);

  // Runs tab: the run in lifecycle words.
  await row.click();
  await page.getByRole('tab', { name: 'Runs' }).click();
  await expect(page.getByRole('tabpanel').getByText(/deleted/i).first()).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole('tabpanel').getByText(/copied/i)).toHaveCount(0);
});
