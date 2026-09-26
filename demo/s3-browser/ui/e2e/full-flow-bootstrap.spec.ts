import { test, expect } from '@playwright/test';

/** Matches `TEST_BOOTSTRAP_PASSWORD` in `tests/common/mod.rs`. */
const TEST_BOOTSTRAP_PASSWORD = 'testpass';

test.describe.configure({ timeout: 120_000 });
// Runs only against a bootstrap-auth proxy (`E2E_AUTH=bootstrap ./scripts/e2e-smoke.sh`).
test.skip(process.env.E2E_AUTH !== 'bootstrap', 'bootstrap-auth flow');

async function signIn(page: import('@playwright/test').Page) {
  await expect(page.getByPlaceholder('Admin password')).toBeVisible({ timeout: 60_000 });
  await page.getByPlaceholder('Admin password').fill(TEST_BOOTSTRAP_PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });
}

test('bootstrap auth: sign in, bucket, upload, list, admin, sign out, sign in, object still visible', async ({
  page,
}) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  const bucketName = `e2e-${Date.now()}`;
  const uploadName = 'e2e-upload.txt';
  const bucketList = page.getByRole('navigation', { name: 'Bucket list' });
  const bucketRowBtn = bucketList.getByRole('button', { name: new RegExp(`^${bucketName}\\b`) });

  // ── Sign in: no auto session when SigV4 auth is on ───────────────
  await page.goto('/_/browse');
  await signIn(page);

  await bucketList.getByRole('button', { name: 'Create bucket' }).click();
  await page.getByRole('textbox', { name: 'Bucket name' }).fill(bucketName);
  await page.getByRole('button', { name: 'Create', exact: true }).click();
  await expect(bucketRowBtn).toBeVisible({ timeout: 30_000 });
  await bucketRowBtn.click();

  // ── Upload (signed by the session's S3 credentials) ──────────────
  await page.getByRole('button', { name: 'Upload Files' }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`Upload to ${bucketName}`) })).toBeVisible({
    timeout: 30_000,
  });
  await page.locator('input[type=file]').first().setInputFiles({
    name: uploadName,
    mimeType: 'text/plain',
    buffer: Buffer.from('deltaglider e2e payload\n'),
  });
  await expect(page.getByText('Upload complete')).toBeVisible({ timeout: 60_000 });
  await page.getByRole('button', { name: /Done — / }).click();
  await expect(page.getByText(uploadName)).toBeVisible({ timeout: 60_000 });

  // ── Admin: the same session reaches the admin API ─────────────────
  await page.goto('/_/admin');
  await expect(page.getByText('Dashboard').first()).toBeVisible({ timeout: 30_000 });

  // ── Unsigned S3 requests are refused ──────────────────────────────
  const anon = await page.request.get(`/${bucketName}/${uploadName}`);
  expect(anon.status()).toBe(403);

  // ── Sign out, sign back in, the object is still there ────────────
  await page.goto(`/_/browse/${bucketName}/`);
  await page.getByRole('button', { name: /Account menu/i }).click();
  await page.getByRole('menuitem', { name: 'Sign out' }).click();
  await page.getByRole('dialog').filter({ hasText: 'Sign out?' }).getByRole('button', { name: 'Sign out' }).click();
  await signIn(page);
  await page.goto(`/_/browse/${bucketName}/`);
  await expect(page.getByText(uploadName)).toBeVisible({ timeout: 30_000 });
});
