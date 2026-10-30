/**
 * QA jobs pass, per-bucket encryption (issue #116 P2): ONE flow through the
 * bucket card's Encryption group, the way an operator sees it. NOT part of
 * the CI merge gate (nightly, through scripts/qa-e2e.sh; without
 * QA_REGRESSION=1 it skips itself).
 *
 * Flow: a bucket with one plaintext object → Storage → Buckets → the
 * bucket's Encryption group → Encrypt with a bucket key (generated key,
 * last 6 characters typed) → apply → the proposal starts the re-encrypt
 * job → the stored files on the backend are ciphertext (old and new
 * objects), and every object reads back through S3 → Rotate key → the new
 * key id shows, the old one is a legacy key, and every object still reads.
 *
 * The proxy is the bootstrap mode of scripts/lib/e2e-proxy.sh (one
 * filesystem backend, data in QA_DATA_DIR): the spec reads the stored files
 * there to see the at-rest form.
 *
 *   QA_REGRESSION=1 QA_ACCESS_KEY=... QA_SECRET_KEY=... QA_DATA_DIR=... \
 *   PLAYWRIGHT_BASE_URL=http://127.0.0.1:18600 \
 *   npx playwright test e2e/qa-jobs-encryption.spec.ts
 */
import { test, expect, type Page } from '@playwright/test';
import { S3Client, CreateBucketCommand, PutObjectCommand, GetObjectCommand } from '@aws-sdk/client-s3';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const PASSWORD = 'testpass';
const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:18600';
const ACCESS_KEY = process.env.QA_ACCESS_KEY ?? 'qa-admin-key';
const SECRET_KEY = process.env.QA_SECRET_KEY ?? 'qa-admin-secret-0123456789';
const DATA_DIR = process.env.QA_DATA_DIR ?? '';
const BUCKET = `qa-enc-${Date.now().toString(36)}`;
const KEY_ID = `${BUCKET}-k1`;
const KEY_ID_2 = `${BUCKET}-k2`;
/** A marker that a plaintext body shows and a ciphertext body hides. */
const MARKER = 'QA-PLAINTEXT-MARKER-0123456789';
const body = (name: string) => Buffer.from(`${MARKER} ${name} `.repeat(200));

test.skip(!process.env.QA_REGRESSION, 'QA pass: run with QA_REGRESSION=1');
test.describe.configure({ mode: 'serial', timeout: 120_000 });
test.use({ actionTimeout: 15_000 });

function s3(): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
  });
}

async function put(key: string): Promise<void> {
  await s3().send(new PutObjectCommand({ Bucket: BUCKET, Key: key, Body: body(key), ContentType: 'image/jpeg' }));
}

async function get(key: string): Promise<Buffer> {
  const out = await s3().send(new GetObjectCommand({ Bucket: BUCKET, Key: key }));
  return Buffer.from(await out.Body!.transformToByteArray());
}

/** The stored file of a passthrough object on the filesystem backend. */
function atRest(key: string): Buffer {
  return readFileSync(join(DATA_DIR, BUCKET, 'deltaspaces', key));
}

async function effective(page: Page): Promise<Record<string, unknown>> {
  const res = await page.request.get('/_/api/admin/buckets');
  expect(res.ok()).toBe(true);
  const list = (await res.json()) as { buckets: { name: string; effective_encryption: Record<string, unknown> }[] };
  return list.buckets.find((b) => b.name === BUCKET)!.effective_encryption;
}

async function waitJobDone(page: Page): Promise<void> {
  await expect
    .poll(
      async () => {
        const res = await page.request.get(`/_/api/admin/jobs/bucket/${BUCKET}`);
        return ((await res.json()) as { active: unknown }).active;
      },
      { timeout: 60_000, intervals: [500] },
    )
    .toBeNull();
}

/** In the open card: fill the key setup and apply it through the review dialog. */
async function applyNewKey(page: Page, keyId: string): Promise<void> {
  const group = page.getByTestId('bucket-encryption-group');
  await group.getByLabel('Key id').fill(keyId);
  const key = await group.getByLabel('Generated key').inputValue();
  expect(key).toMatch(/^[0-9a-f]{64}$/);
  const review = group.getByRole('button', { name: 'Review & apply' });
  await expect(review).toBeDisabled();
  await group.getByLabel('Last 6 characters of the key').fill(key.slice(-6));
  await expect(review).toBeEnabled();
  await review.click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Review changes before applying' });
  await expect(dialog).toContainText('Only new writes change');
  await dialog.getByRole('button', { name: /Apply and persist/i }).click();
  await expect(dialog).toBeHidden();
}

test('a bucket key from the bucket card: ciphertext at rest, re-encrypt, rotate', async ({ browser }) => {
  expect(DATA_DIR, 'QA_DATA_DIR names the data dir of the filesystem backend').not.toBe('');
  const pageErrors: string[] = [];
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 1000 } });
  const page = await ctx.newPage();
  page.on('pageerror', (e) => pageErrors.push(e.message));

  await s3().send(new CreateBucketCommand({ Bucket: BUCKET }));
  await put('before.jpg');
  expect(atRest('before.jpg').includes(MARKER), 'the first object is plaintext at rest').toBe(true);

  // Sign in.
  await page.goto('/_/browse');
  await page.getByLabel('Admin password', { exact: true }).fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });

  // Storage → Buckets → the bucket → Encryption → Encrypt with a bucket key.
  await page.goto('/_/admin/storage/buckets');
  // The row's name is its visible text: the bucket name, then the chips.
  const row = page.getByRole('button', { name: new RegExp(`^${BUCKET} `), expanded: false });
  await expect(row).toContainText('Not encrypted', { timeout: 30_000 });
  await row.click();
  const group = page.getByTestId('bucket-encryption-group');
  await expect(group).toBeVisible();
  await group.getByText('Encrypt with a bucket key', { exact: true }).click();
  await applyNewKey(page, KEY_ID);

  // The proposal: encrypt the existing object of this bucket.
  const proposal = page.getByRole('dialog').filter({ hasText: 'Encrypt existing objects?' });
  await expect(proposal).toBeVisible();
  await expect(proposal.getByRole('checkbox', { name: BUCKET })).toBeChecked();
  await proposal.getByRole('button', { name: /^Start now/ }).click();
  await waitJobDone(page);

  const e1 = await effective(page);
  expect(e1).toMatchObject({ source: 'bucket', mode: 'aes256-gcm-proxy', key_id: KEY_ID, encrypts: true });
  await expect(page.getByText(`Encrypted · bucket key ${KEY_ID}`).first()).toBeVisible();

  // New and old objects are ciphertext at rest, and read back as plaintext.
  await put('after.jpg');
  for (const key of ['before.jpg', 'after.jpg']) {
    expect(atRest(key).includes(MARKER), `${key} is ciphertext at rest`).toBe(false);
    expect((await get(key)).equals(body(key)), `${key} reads back`).toBe(true);
  }

  // Rotate: a new key and key id; the old key becomes the legacy key.
  await page.reload();
  await page.getByRole('button', { name: new RegExp(`^${BUCKET} `), expanded: false }).click();
  await page.getByTestId('bucket-encryption-group').getByRole('button', { name: /Rotate key/ }).click();
  await applyNewKey(page, KEY_ID_2);
  const rotate = page.getByRole('dialog').filter({ hasText: 'Re-encrypt existing objects with the new key?' });
  await expect(rotate).toBeVisible();
  await rotate.getByRole('button', { name: 'Later' }).click();

  const e2 = await effective(page);
  expect(e2).toMatchObject({ source: 'bucket', key_id: KEY_ID_2, legacy_key_id: KEY_ID });
  await expect(page.getByText(`Encrypted · bucket key ${KEY_ID_2}`).first()).toBeVisible();
  // The banner of the bucket's legacy key, with its per-bucket check.
  await expect(page.getByTestId('bucket-encryption-group').getByText('Decrypt-only shim active')).toBeVisible();
  await put('rotated.jpg');
  for (const key of ['before.jpg', 'after.jpg', 'rotated.jpg']) {
    expect(atRest(key).includes(MARKER), `${key} is ciphertext at rest`).toBe(false);
    expect((await get(key)).equals(body(key)), `${key} reads back after the rotation`).toBe(true);
  }

  expect(pageErrors).toEqual([]);
  await ctx.close();
});
