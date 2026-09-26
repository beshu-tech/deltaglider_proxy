/**
 * Reproductions of the exploratory-test findings (2026-09-26). NOT part of
 * the CI merge gate: without EXPLORE=1 every test skips, so
 * `playwright test e2e/` in scripts/e2e-smoke.sh does not run them.
 *
 * Each test states the EXPECTED behaviour, so it FAILS while the finding is
 * open and passes once it is fixed.
 *
 * Run against a proxy in IAM mode with two named backends: an encrypted
 * filesystem backend and a second backend (an S3 backend with
 * `allow_local: true`, for example a local MinIO). Sign in with an IAM admin
 * key pair; the bootstrap SigV4 pair must be set too:
 *
 *   EXPLORE=1 EXPLORE_ACCESS_KEY=... EXPLORE_SECRET_KEY=... \
 *   EXPLORE_BOOTSTRAP_KEY=... EXPLORE_BOOTSTRAP_SECRET=... \
 *   EXPLORE_BACKEND_A=local-disk EXPLORE_BACKEND_B=hetzner-fsn1 \
 *   PLAYWRIGHT_BASE_URL=http://127.0.0.1:19600 npx playwright test e2e/explore/
 *
 * Optional:
 *   EXPLORE_SYNC_BUCKET=<config_sync_bucket>   the coordination-bucket test
 *   EXPLORE_LOCK_BASE=<url of a bootstrap-mode proxy started with
 *     DGP_RATE_LIMIT_MAX_ATTEMPTS=3>           the lockout-message test
 *   EXPLORE_PASSWORD=<its bootstrap password>
 *   EXPLORE_DESTRUCTIVE=1                      the key-rotation test. It
 *     REPLACES the encryption key of EXPLORE_BACKEND_A: run it only on a
 *     throwaway proxy.
 *
 * The tests create their own buckets (unique name per run) and restore the
 * admission section they change.
 */
import { test, expect, type Page, type APIRequestContext } from '@playwright/test';
import {
  S3Client,
  CreateBucketCommand,
  PutObjectCommand,
  GetObjectCommand,
  DeleteObjectCommand,
  ListObjectsV2Command,
} from '@aws-sdk/client-s3';
import { randomBytes } from 'node:crypto';

const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:19600';
const ACCESS_KEY = process.env.EXPLORE_ACCESS_KEY ?? '';
const SECRET_KEY = process.env.EXPLORE_SECRET_KEY ?? '';
const BACKEND_A = process.env.EXPLORE_BACKEND_A ?? 'local-disk';
const BACKEND_B = process.env.EXPLORE_BACKEND_B ?? 'hetzner-fsn1';
const RUN = Date.now().toString(36);
const CSRF = { 'x-requested-with': 'XMLHttpRequest' };

test.skip(!process.env.EXPLORE, 'Exploratory reproductions: run with EXPLORE=1');
test.describe.configure({ timeout: 180_000 });
test.use({ viewport: { width: 1440, height: 900 }, actionTimeout: 15_000 });

function s3(ak = ACCESS_KEY, sk = SECRET_KEY): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ak, secretAccessKey: sk },
    // One attempt: a 500 must surface as a 500, not as four retries.
    maxAttempts: 1,
  });
}

async function signIn(page: Page) {
  await page.goto('/_/admin');
  await page.getByPlaceholder('Access Key ID').fill(ACCESS_KEY);
  await page.getByPlaceholder('Secret Access Key').fill(SECRET_KEY);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByText('ADMIN SETTINGS', { exact: false })).toBeVisible({ timeout: 30_000 });
}

async function httpStatus(p: Promise<unknown>): Promise<number> {
  try {
    await p;
    return 200;
  } catch (e) {
    return (e as { $metadata?: { httpStatusCode?: number } }).$metadata?.httpStatusCode ?? -1;
  }
}

/** Starts a migrate job and waits until the bucket has no active job. */
async function migrate(req: APIRequestContext, bucket: string, target: string) {
  const r = await req.post(`/_/api/admin/buckets/${bucket}/migrate`, {
    headers: CSRF,
    data: { target_backend: target, delete_source: false },
  });
  expect(r.status(), await r.text()).toBe(202);
  await expect
    .poll(async () => (await (await req.get(`/_/api/admin/jobs/bucket/${bucket}`)).json()).active, {
      timeout: 120_000,
      intervals: [1_000],
    })
    .toBeNull();
}

test.beforeAll(() => {
  expect(ACCESS_KEY, 'set EXPLORE_ACCESS_KEY').not.toBe('');
});

// ── Storage / Backends ──────────────────────────────────────────────────

test('a Backends-page edit keeps allow_local on the other backends', async ({ page }) => {
  await signIn(page);
  const before = await (await page.request.get('/_/api/admin/config/section/storage')).json();
  const localBackends = (before.backends ?? [])
    .filter((b: { allow_local?: boolean }) => b.allow_local)
    .map((b: { name: string }) => b.name);
  test.skip(localBackends.length === 0, 'needs a backend with allow_local: true');

  // Capture the storage PUT and abort it, so the test changes nothing.
  let body: { backends?: { name: string; allow_local?: boolean }[] } | null = null;
  await page.route('**/_/api/admin/config/section/storage', async (route) => {
    if (route.request().method() === 'PUT') {
      body = route.request().postDataJSON();
      await route.abort();
    } else {
      await route.continue();
    }
  });
  await page.goto('/_/admin/storage/backends');
  await page.getByRole('button', { name: /Rotate key/ }).first().click();
  await page.getByRole('checkbox').first().check();
  await page.locator('button:has-text("Apply")').first().click();
  await expect.poll(() => body !== null, { timeout: 10_000 }).toBe(true);
  for (const name of localBackends) {
    const sent = body!.backends?.find((b) => b.name === name);
    // Without allow_local the server refuses the http:// endpoint (422), or,
    // when DGP_BACKEND_ALLOW_LOCAL is set, persists the YAML without it.
    expect(sent?.allow_local, `allow_local dropped from backend ${name}`).toBe(true);
  }
});

test('Rotate key keeps existing objects readable, and a job with only failures is not "succeeded"', async ({ page }) => {
  test.skip(!process.env.EXPLORE_DESTRUCTIVE, 'replaces an encryption key: set EXPLORE_DESTRUCTIVE=1');
  const bucket = `explore-rot-${RUN}`;
  const c = s3();
  await c.send(new CreateBucketCommand({ Bucket: bucket }));
  await signIn(page);
  // Route the bucket to the encrypted backend, then write an object.
  const put = await page.request.put('/_/api/admin/config/section/storage', {
    headers: CSRF,
    data: { buckets: { [bucket]: { backend: BACKEND_A } } },
  });
  expect(put.ok(), await put.text()).toBe(true);
  await c.send(new PutObjectCommand({ Bucket: bucket, Key: 'old.txt', Body: 'written under the old key' }));

  await page.goto('/_/admin/storage/backends');
  await page.getByRole('button', { name: /Rotate key/ }).first().click();
  // The UI promises "Reads work transparently for both old and new objects".
  await expect(page.getByText('Reads work transparently for both old and new objects')).toBeVisible();
  await page.getByRole('checkbox').first().check();
  await page.locator('button:has-text("Apply")').first().click();
  await expect(page.getByText('Re-encrypt existing objects with the new key?')).toBeVisible({ timeout: 15_000 });

  expect(await httpStatus(c.send(new GetObjectCommand({ Bucket: bucket, Key: 'old.txt' }))), 'old object after rotation').toBe(200);

  // The re-encrypt job for the bucket: every object fails, so the job must
  // not end as "succeeded".
  const r = await page.request.post('/_/api/admin/jobs/reencrypt', { headers: CSRF, data: { buckets: [bucket] } });
  const jobId = (await r.json()).started[0].job_id;
  await expect
    .poll(async () => (await (await page.request.get(`/_/api/admin/jobs/bucket/${bucket}`)).json()).active, { timeout: 60_000 })
    .toBeNull();
  const job = (await (await page.request.get('/_/api/admin/jobs')).json()).jobs.find(
    (j: { id: string }) => j.id === `maintenance:${jobId}`,
  );
  if (job.progress.failed > 0 && job.progress.processed === 0) {
    expect(job.status, `job with ${job.progress.failed} failures and 0 processed`).not.toBe('succeeded');
  }
});

// ── Maintenance: migrate ────────────────────────────────────────────────

test('migrating back to a backend that holds an old copy does not bring deleted objects back', async ({ page }) => {
  const bucket = `explore-mig-${RUN}`;
  const c = s3();
  await c.send(new CreateBucketCommand({ Bucket: bucket }));
  await c.send(new PutObjectCommand({ Bucket: bucket, Key: 'keep.txt', Body: 'keep' }));
  await c.send(new PutObjectCommand({ Bucket: bucket, Key: 'gone.txt', Body: 'delete me later' }));
  await signIn(page);
  const route = await page.request.put('/_/api/admin/config/section/storage', {
    headers: CSRF,
    data: { buckets: { [bucket]: { backend: BACKEND_A } } },
  });
  expect(route.ok(), await route.text()).toBe(true);

  // A → B with the default "keep a safety copy" (delete_source: false).
  await migrate(page.request, bucket, BACKEND_B);
  await c.send(new DeleteObjectCommand({ Bucket: bucket, Key: 'gone.txt' }));
  // B → A: A still holds the safety copy from the first move.
  await migrate(page.request, bucket, BACKEND_A);

  const list = await c.send(new ListObjectsV2Command({ Bucket: bucket }));
  const keys = (list.Contents ?? []).map((o) => o.Key);
  expect(keys, 'an object deleted before the move came back').not.toContain('gone.txt');
  expect(keys).toContain('keep.txt');
});

// ── Admission ───────────────────────────────────────────────────────────

test('an allow-anonymous request rule lets the anonymous request through, as the trace says', async ({ page }) => {
  const bucket = `explore-adm-${RUN}`;
  const c = s3();
  await c.send(new CreateBucketCommand({ Bucket: bucket }));
  await c.send(new PutObjectCommand({ Bucket: bucket, Key: 'builds/app.zip', Body: randomBytes(64) }));
  await signIn(page);
  const original = await (await page.request.get('/_/api/admin/config/section/admission')).json();
  const blocks = [
    {
      name: `explore-allow-zips-${RUN}`,
      match: { method: ['GET', 'HEAD'], bucket, path_glob: '*.zip' },
      action: 'allow-anonymous',
    },
    ...(original.blocks ?? []),
  ];
  const put = await page.request.put('/_/api/admin/config/section/admission', { headers: CSRF, data: { blocks } });
  expect(put.ok(), await put.text()).toBe(true);
  try {
    const trace = await (
      await page.request.post('/_/api/admin/config/trace', {
        headers: CSRF,
        data: { method: 'GET', path: `/${bucket}/builds/app.zip`, authenticated: false },
      })
    ).json();
    expect(trace.admission.decision).toBe('allow-anonymous');
    // The documented example ("allow-public-zips") and the trace both say
    // the zip is anonymously readable. The real request must agree.
    const anon = await page.request.get(`${BASE}/${bucket}/builds/app.zip`, { headers: { cookie: '' } });
    expect(anon.status(), 'anonymous GET after an allow-anonymous decision').toBe(200);
  } finally {
    await page.request.put('/_/api/admin/config/section/admission', {
      headers: CSRF,
      data: { blocks: original.blocks ?? [] },
    });
  }
});

// ── Access: Credentials page, lockout ───────────────────────────────────

test('the Credentials page shows the configured bootstrap access key id', async ({ page }) => {
  await signIn(page);
  const cfg = await (await page.request.get('/_/api/admin/config')).json();
  test.skip(!cfg.access_key_id, 'needs a bootstrap SigV4 pair');
  await page.goto('/_/admin/access/credentials');
  // The field is empty today, and the help text says "To remove them, clear
  // both fields" — so an operator reads an empty field as "no bootstrap key".
  await expect(page.getByPlaceholder('AKIAIOSFODNN7EXAMPLE')).toHaveValue(cfg.access_key_id, { timeout: 15_000 });
});

test('a locked-out sign-in says so instead of "Login failed: Login failed"', async ({ browser }) => {
  const lockBase = process.env.EXPLORE_LOCK_BASE;
  test.skip(!lockBase, 'needs EXPLORE_LOCK_BASE (bootstrap proxy with DGP_RATE_LIMIT_MAX_ATTEMPTS=3)');
  const ctx = await browser.newContext({ baseURL: lockBase });
  const page = await ctx.newPage();
  await page.goto('/_/browse');
  for (let i = 0; i < 4; i++) {
    await page.getByPlaceholder('Admin password').fill(`wrong-${i}`);
    const resp = page.waitForResponse((r) => r.url().endsWith('/_/api/admin/login'));
    await page.getByRole('button', { name: 'Sign in' }).click();
    await resp;
  }
  // The 4th attempt is a 429. The correct password is also refused while
  // the lockout lasts; the user must learn that it is a lockout.
  await page.getByPlaceholder('Admin password').fill(process.env.EXPLORE_PASSWORD ?? 'wrong');
  const last = page.waitForResponse((r) => r.url().endsWith('/_/api/admin/login'));
  await page.getByRole('button', { name: 'Sign in' }).click();
  expect((await last).status()).toBe(429);
  await expect(page.getByText(/too many|locked|try again in/i)).toBeVisible({ timeout: 5_000 });
  await ctx.close();
});

// ── Coordination bucket ─────────────────────────────────────────────────

test('S3 clients cannot write the coordination (config sync) bucket', async () => {
  const bucket = process.env.EXPLORE_SYNC_BUCKET;
  test.skip(!bucket, 'needs EXPLORE_SYNC_BUCKET');
  // An object written here can replace the synced IAM database (a rollback
  // that re-enables a disabled key on every peer) or a lease.
  const status = await httpStatus(
    s3().send(new PutObjectCommand({ Bucket: bucket, Key: `_dgp/explore-probe-${RUN}.txt`, Body: 'x' })),
  );
  expect(status, 'PUT into the coordination bucket').toBe(403);
});

// ── Upload page ─────────────────────────────────────────────────────────

test('the upload page sends 300 tiny files in under a minute', async ({ page }) => {
  const bucket = `explore-bulk-${RUN}`;
  await s3().send(new CreateBucketCommand({ Bucket: bucket }));
  await signIn(page);
  await page.goto(`/_/browse/${bucket}/`);
  await page.getByRole('button', { name: 'Upload Files' }).first().click();
  const files = Array.from({ length: 300 }, (_, i) => ({
    name: `bulk-${String(i).padStart(4, '0')}.txt`,
    mimeType: 'text/plain',
    buffer: Buffer.from(`file ${i}`),
  }));
  await page.locator('input[type=file]').first().setInputFiles(files);
  // Each PUT takes about 10 ms on the server. Today the page re-renders the
  // whole queue on every progress event and spends about 1.7 s per file, so
  // 300 files take about 9 minutes.
  await expect(page.getByText('Upload complete')).toBeVisible({ timeout: 60_000 });
});

test('the upload page sends 1000 tiny files with a bounded heap', async ({ page }) => {
  const bucket = `explore-bulk1k-${RUN}`;
  await s3().send(new CreateBucketCommand({ Bucket: bucket }));
  await signIn(page);
  await page.goto(`/_/browse/${bucket}/`);
  await page.getByRole('button', { name: 'Upload Files' }).first().click();
  const files = Array.from({ length: 1000 }, (_, i) => ({
    name: `k-${String(i).padStart(4, '0')}.txt`,
    mimeType: 'text/plain',
    buffer: Buffer.from(`file ${i}`),
  }));
  const t0 = Date.now();
  await page.locator('input[type=file]').first().setInputFiles(files);
  await expect(page.getByText('Upload complete')).toBeVisible({ timeout: 180_000 });
  const secs = (Date.now() - t0) / 1000;
  // Only the rows in view are in the DOM (the list is virtualised).
  expect(await page.getByRole('listitem').count()).toBeLessThan(60);
  const heap = await page.evaluate(
    () => (performance as Performance & { memory?: { usedJSHeapSize: number } }).memory?.usedJSHeapSize ?? 0,
  );
  console.log(`1000 files in ${secs.toFixed(1)} s, JS heap ${(heap / 1e6).toFixed(0)} MB`);
  expect(heap, 'JS heap after 1000 uploads').toBeLessThan(300e6);
});

// ── S3 transparency: keys ───────────────────────────────────────────────

test('reserved and normalised keys behave as documented, and a refusal names the rule', async () => {
  // Lead decision #13: the proxy keeps its reserved names (it stores its
  // own files under them) and documents them; the refusal names the rule.
  const bucket = `explore-keys-${RUN}`;
  const c = s3();
  await c.send(new CreateBucketCommand({ Bucket: bucket }));
  for (const key of ['docs/reference.bin', 'backups/db.sql.delta']) {
    type S3Err = { $metadata?: { httpStatusCode?: number }; message?: string };
    const err: S3Err | null = await c
      .send(new PutObjectCommand({ Bucket: bucket, Key: key, Body: 'x' }))
      .then(() => null, (e: unknown) => e as S3Err);
    expect(err?.$metadata?.httpStatusCode, key).toBe(400);
    expect(err?.message, key).toContain(`'${key}' is refused`);
    expect(err?.message, key).toContain('s3-api-compatibility#reserved-and-normalised-keys');
  }
  // Documented normalisation: the leading slash is removed.
  await c.send(new PutObjectCommand({ Bucket: bucket, Key: '/leading.txt', Body: 'x' }));
  const keys = ((await c.send(new ListObjectsV2Command({ Bucket: bucket }))).Contents ?? []).map((o) => o.Key);
  expect(keys).toContain('leading.txt');
});

test('a 250-character file name is stored, or refused with a 4xx, never a 500', async () => {
  const bucket = `explore-long-${RUN}`;
  const c = s3();
  await c.send(new CreateBucketCommand({ Bucket: bucket }));
  // Delta-eligible: the filesystem backend appends ".delta" and passes the
  // 255-byte file-name limit.
  const key = `${'g'.repeat(250)}.zip`;
  const status = await httpStatus(c.send(new PutObjectCommand({ Bucket: bucket, Key: key, Body: randomBytes(1024) })));
  expect(status === 200 || (status >= 400 && status < 500), `status ${status}`).toBe(true);
});
