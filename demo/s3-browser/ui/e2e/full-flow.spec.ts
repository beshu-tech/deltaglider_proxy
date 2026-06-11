import { test, expect } from '@playwright/test';

/** Matches `TEST_BOOTSTRAP_PASSWORD` in `tests/common/mod.rs`. */
const TEST_BOOTSTRAP_PASSWORD = 'testpass';

test.describe.configure({ timeout: 120_000 });

test('open auth: bucket, upload, list, admin login, sign out, reconnect, object still visible', async ({
  page,
}) => {
  test.setTimeout(120_000);
  await page.setViewportSize({ width: 1400, height: 900 });

  const bucketName = `e2e-${Date.now()}`;
  const uploadName = 'e2e-upload.txt';

  // ── Browse (open-mode auto session) ─────────────────────────────
  await page.goto('/_/browse');
  await expect(page.getByRole('button', { name: 'Create bucket' })).toBeVisible({ timeout: 60_000 });

  await page.getByRole('button', { name: 'Create bucket' }).click();
  await page.getByRole('textbox', { name: 'Bucket name' }).fill(bucketName);
  await page.getByRole('button', { name: 'Create', exact: true }).click();
  const bucketRowBtn = page.getByRole('button', { name: `${bucketName} S3 backend` });
  await expect(bucketRowBtn).toBeVisible({ timeout: 30_000 });
  await bucketRowBtn.click();

  // ── Upload page ─────────────────────────────────────────────────
  await page.getByRole('button', { name: 'Upload Files' }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`Upload to ${bucketName}`) })).toBeVisible({
    timeout: 30_000,
  });

  await page.locator('input[type=file]').first().setInputFiles({
    name: uploadName,
    mimeType: 'text/plain',
    buffer: Buffer.from('deltaglider e2e payload\n'),
  });

  // Upload completes near-instantly for a tiny file; the page swaps the
  // "Back to browse" button for the success panel's single Done action.
  await expect(page.getByText('Upload complete')).toBeVisible({ timeout: 60_000 });
  await page.getByRole('button', { name: /Done — go to/ }).click();
  await expect(page.getByText(uploadName)).toBeVisible({ timeout: 60_000 });

  // ── Admin (bootstrap password) ──────────────────────────────────
  await page.goto('/_/admin');
  await expect(page.getByRole('textbox', { name: 'Bootstrap password' })).toBeVisible({ timeout: 30_000 });
  await page.getByRole('textbox', { name: 'Bootstrap password' }).fill(TEST_BOOTSTRAP_PASSWORD);
  await page.getByRole('button', { name: 'Sign In' }).click();
  await expect(page.getByText('Dashboard').first()).toBeVisible({ timeout: 30_000 });

  await page.getByRole('button', { name: 'Browser' }).click();
  await expect(page.getByRole('button', { name: 'Create bucket' })).toBeVisible({ timeout: 30_000 });

  // ── Sign out (open mode → reconnect gate) ─────────────────────────
  page.once('dialog', (d) => d.accept());
  await page.getByRole('button', { name: /Account menu/i }).click();
  await page.getByRole('menuitem', { name: 'Sign out' }).click();

  await expect(page.getByRole('button', { name: 'Connect again' })).toBeVisible({ timeout: 30_000 });
  await page.getByRole('button', { name: 'Connect again' }).click();

  await expect(page.getByRole('button', { name: 'Create bucket' })).toBeVisible({ timeout: 60_000 });
  await page.getByRole('button', { name: `${bucketName} S3 backend` }).click();
  await expect(page.getByText(uploadName)).toBeVisible({ timeout: 30_000 });
});
