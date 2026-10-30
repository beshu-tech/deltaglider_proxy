/**
 * QA jobs pass, maintenance kind: ONE flow through a bucket migration, the
 * way an operator sees it. NOT part of the CI merge gate (nightly, like
 * qa-regression.spec.ts; without QA_REGRESSION=1 it skips itself).
 *
 * Flow: seed a bucket through the S3 API → Jobs → New job → Migrate bucket
 * in the UI → a second page on that bucket's browser view shows the busy
 * banner → the Jobs row shows progress while the job runs → the job ends
 * succeeded → the bucket lists every object from its new backend.
 *
 * The proxy needs TWO named backends (a migrate moves a bucket between
 * backends). The bootstrap mode of scripts/lib/e2e-proxy.sh has one
 * (`storage.filesystem`), so this spec needs a proxy started with:
 *
 *   storage:
 *     backends:
 *       - { name: local-disk, type: filesystem, path: "$E2E_DIR/data" }
 *       - { name: qa-target,  type: filesystem, path: "$E2E_DIR/qa-target" }
 *     default_backend: local-disk
 *     buckets:
 *       qa-public: { public_prefixes: ["pub/"] }
 *
 * plus the same bootstrap SigV4 pair and admin password `testpass`.
 * QA_MIGRATE_TARGET names the target backend (default `qa-target`).
 *
 *   QA_REGRESSION=1 QA_ACCESS_KEY=... QA_SECRET_KEY=... \
 *   PLAYWRIGHT_BASE_URL=http://127.0.0.1:18600 \
 *   npx playwright test e2e/qa-jobs-maintenance.spec.ts
 *
 * Run order: scripts/qa-e2e.sh gives each spec its own proxy, so the job
 * this spec starts never shows up in another spec's Jobs list.
 */
import { test, expect, type Locator, type Page } from '@playwright/test';
import { S3Client, CreateBucketCommand, PutObjectCommand, ListObjectsV2Command } from '@aws-sdk/client-s3';

const PASSWORD = 'testpass';
const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:18600';
const ACCESS_KEY = process.env.QA_ACCESS_KEY ?? 'qa-admin-key';
const SECRET_KEY = process.env.QA_SECRET_KEY ?? 'qa-admin-secret-0123456789';
const TARGET = process.env.QA_MIGRATE_TARGET ?? 'qa-target';
const BUCKET = `qa-mig-${Date.now().toString(36)}`;
/** Enough objects that the copy and verify phases last a few seconds. */
const OBJECTS = 1500;
const OBJECT_BYTES = 64 * 1024;
const SEED_CONCURRENCY = 16;

test.skip(!process.env.QA_REGRESSION, 'QA pass: run with QA_REGRESSION=1');
test.describe.configure({ mode: 'serial', timeout: 90_000 });
test.use({ actionTimeout: 15_000 });

function s3(): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
  });
}

const keyOf = (i: number) => `obj-${String(i).padStart(5, '0')}.txt`;

async function seed(): Promise<void> {
  const client = s3();
  await client.send(new CreateBucketCommand({ Bucket: BUCKET }));
  let next = 0;
  const worker = async () => {
    while (next < OBJECTS) {
      const i = next++;
      // Distinct bytes per object: no two objects share a body.
      const body = Buffer.alloc(OBJECT_BYTES, `${i}:`);
      await client.send(new PutObjectCommand({ Bucket: BUCKET, Key: keyOf(i), Body: body }));
    }
  };
  await Promise.all(Array.from({ length: SEED_CONCURRENCY }, worker));
}

async function countKeys(): Promise<number> {
  const client = s3();
  let token: string | undefined;
  let n = 0;
  do {
    const out = await client.send(new ListObjectsV2Command({ Bucket: BUCKET, ContinuationToken: token }));
    n += out.KeyCount ?? 0;
    token = out.IsTruncated ? out.NextContinuationToken : undefined;
  } while (token);
  return n;
}

interface JobRowLite {
  id: string;
  kind: string;
  name: string;
  status: string;
}

async function migrateRow(page: Page): Promise<JobRowLite | undefined> {
  const res = await page.request.get('/_/api/admin/jobs');
  expect(res.ok()).toBe(true);
  const body = (await res.json()) as { jobs: JobRowLite[] };
  return body.jobs.find((j) => j.kind === 'migrate' && j.name === BUCKET);
}

/** Pick an AntD Select option by the start of its label. */
async function pickOption(page: Page, root: Locator, combobox: string, labelStart: string) {
  await root.getByRole('combobox', { name: combobox }).click();
  await page
    .locator('.ant-select-dropdown:visible .ant-select-item-option')
    .filter({ hasText: new RegExp(`^${labelStart.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`) })
    .first()
    .click();
}

test('migrate a bucket from the UI: busy banner, live row, succeeded, listing works', async ({ browser }) => {
  const pageErrors: string[] = [];
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  const page = await ctx.newPage();
  page.on('pageerror', (e) => pageErrors.push(e.message));

  // Sign in.
  await page.goto('/_/browse');
  await page.getByLabel('Admin password', { exact: true }).fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });

  // The proxy has the target backend (see the header).
  const backends = (await (await page.request.get('/_/api/admin/backends')).json()) as {
    backends: { name: string }[];
  };
  expect(
    backends.backends.map((b) => b.name),
    `this spec needs a second named backend '${TARGET}' (see the spec header)`,
  ).toContain(TARGET);

  await seed();
  expect(await countKeys()).toBe(OBJECTS);

  // Jobs → New job → Migrate bucket (the Jobs page lists every run).
  await page.goto('/_/admin/jobs');
  await expect(page.getByRole('heading', { level: 2, name: /^Jobs\b/ })).toBeVisible({ timeout: 30_000 });
  await page.getByRole('button', { name: 'New job' }).click();
  await page.getByRole('menuitem', { name: /^Migrate bucket/ }).click();
  // The title names the bucket once it is picked: find the dialog by its button.
  const dialog = page.getByRole('dialog').filter({ has: page.getByRole('button', { name: 'Start migration' }) });
  await expect(dialog).toBeVisible();
  await pickOption(page, dialog, 'Bucket', `${BUCKET} (on `);
  await pickOption(page, dialog, 'Target backend', `${TARGET} (`);
  // Both backends are plaintext filesystems: no acknowledgement to give.
  await dialog.getByRole('button', { name: 'Start migration' }).click();
  await expect(dialog).toBeHidden();

  // A second page on the bucket's browser view shows the busy banner.
  const browserPage = await ctx.newPage();
  browserPage.on('pageerror', (e) => pageErrors.push(e.message));
  await browserPage.goto(`/_/browse/${BUCKET}/`);
  await expect(browserPage.getByText(/Migrating this bucket to another backend/)).toBeVisible({ timeout: 10_000 });

  // The Jobs row shows the job while it runs (the banner above proved it
  // active): its status cell names a phase or counts objects.
  const jobRow = page.getByRole('row').filter({ hasText: BUCKET });
  await expect(jobRow).toBeVisible({ timeout: 10_000 });
  await expect(jobRow.locator('[data-col="status"]')).toContainText(/\d|stag|copy|verif|switch|clean/i);

  // The job ends succeeded (the API is the authority on the status word).
  await expect
    .poll(async () => (await migrateRow(page))?.status, { timeout: 60_000, intervals: [500] })
    .toBe('succeeded');
  // The row stops looking active.
  await expect(jobRow).not.toContainText(/running|queued|cancelling/i, { timeout: 15_000 });

  // The bucket lists every object from its new backend, through S3 and the UI.
  expect(await countKeys()).toBe(OBJECTS);
  const origins = (await (await page.request.get('/_/api/admin/buckets')).json()) as {
    buckets: { name: string; backend_name?: string | null }[];
  };
  expect(origins.buckets.find((b) => b.name === BUCKET)?.backend_name).toBe(TARGET);
  await browserPage.reload();
  await expect(browserPage.getByTestId(`object-row-${keyOf(0)}`)).toBeVisible({ timeout: 30_000 });
  await expect(browserPage.getByText(/Migrating this bucket to another backend/)).toBeHidden();

  expect(pageErrors).toEqual([]);
  await ctx.close();
});
