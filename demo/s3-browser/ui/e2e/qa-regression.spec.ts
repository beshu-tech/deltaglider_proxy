/**
 * QA regression pass: drives the real admin UI + file browser like a user.
 * NOT part of the CI merge gate.
 *
 * Run locally against a proxy in bootstrap mode (filesystem backend, bootstrap
 * SigV4 creds in `access:`, bootstrap password `testpass`, and a bucket policy
 * `qa-public: { public_prefixes: ["pub/"] }`):
 *
 *   QA_ACCESS_KEY=... QA_SECRET_KEY=... \
 *   PLAYWRIGHT_BASE_URL=http://127.0.0.1:18600 npm run e2e:qa
 *
 * Without QA_REGRESSION=1 (set by `e2e:qa`) the spec skips itself, so the CI
 * smoke run (`playwright test e2e/`) does not pick it up.
 *
 * The spec seeds its own data (unique bucket names per run) and fails on any
 * console error or 4xx/5xx response that a step does not expect.
 */
import { test, expect, type Page, type Response } from '@playwright/test';
import { getSignedUrl } from '@aws-sdk/s3-request-presigner';
import { readFileSync } from 'node:fs';
import {
  S3Client,
  CreateBucketCommand,
  PutObjectCommand,
  ListObjectsV2Command,
  GetObjectCommand,
} from '@aws-sdk/client-s3';

const PASSWORD = 'testpass';
const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:18600';
const ACCESS_KEY = process.env.QA_ACCESS_KEY ?? 'qa-admin-key';
const SECRET_KEY = process.env.QA_SECRET_KEY ?? 'qa-admin-secret-0123456789';
const PUBLIC_BUCKET = process.env.QA_PUBLIC_BUCKET ?? 'qa-public';
const RUN = Date.now().toString(36);
const BUCKET = `qa-${RUN}`;
const BUCKET2 = `qa-${RUN}-dst`;

test.skip(!process.env.QA_REGRESSION, 'QA pass: run with `npm run e2e:qa`');
test.describe.configure({ mode: 'serial', timeout: 180_000 });
test.use({ actionTimeout: 15_000 });

// ── Error collection ────────────────────────────────────────────────────

interface Expected {
  status: number;
  url: RegExp;
  method?: string;
}

/**
 * Records console errors and failed responses. A step announces the failures
 * it provokes on purpose with `expectFailure`; everything else fails the test.
 */
class Watch {
  consoleErrors: string[] = [];
  failed: { status: number; method: string; url: string }[] = [];
  private expected: Expected[] = [];

  constructor(page: Page) {
    page.on('console', (m) => {
      if (m.type() !== 'error') return;
      const text = m.text();
      // The browser logs every non-2xx response like this; responses are
      // checked (with their allow-list) below, so do not count them twice.
      if (text.startsWith('Failed to load resource')) return;
      this.consoleErrors.push(text);
    });
    page.on('pageerror', (e) => this.consoleErrors.push(`pageerror: ${e.message}`));
    page.on('response', (r: Response) => {
      if (r.status() < 400) return;
      this.failed.push({ status: r.status(), method: r.request().method(), url: r.url() });
    });
  }

  expectFailure(status: number, url: RegExp, method?: string) {
    this.expected.push({ status, url, method });
  }

  unexpected() {
    return this.failed.filter(
      (f) =>
        !this.expected.some(
          (e) => e.status === f.status && e.url.test(f.url) && (!e.method || e.method === f.method),
        ),
    );
  }

  assertClean(label: string) {
    expect.soft(this.consoleErrors, `${label}: console errors`).toEqual([]);
    expect.soft(this.unexpected(), `${label}: unexpected failed requests`).toEqual([]);
    this.consoleErrors = [];
    this.failed = [];
    this.expected = [];
  }
}

function s3(): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
  });
}

async function listKeys(bucket: string, prefix = ''): Promise<string[]> {
  const out = await s3().send(new ListObjectsV2Command({ Bucket: bucket, Prefix: prefix }));
  return (out.Contents ?? []).map((o) => o.Key!).sort();
}

async function getText(bucket: string, key: string): Promise<string> {
  const out = await s3().send(new GetObjectCommand({ Bucket: bucket, Key: key }));
  return out.Body!.transformToString();
}

// ── Shared page (one browser session across the serial steps) ───────────

let page: Page;
let watch: Watch;

test.beforeAll(async ({ browser }) => {
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 }, acceptDownloads: true });
  // Headless Chromium exposes showSaveFilePicker but has no dialog to answer
  // it; drop it so the ZIP download takes the <a download> path, which
  // Playwright can capture as a download event.
  await ctx.addInitScript(() => {
    delete (window as unknown as { showSaveFilePicker?: unknown }).showSaveFilePicker;
  });
  page = await ctx.newPage();
  watch = new Watch(page);
});

test.afterEach(async ({}, info) => {
  watch.assertClean(info.title);
});

test.afterAll(async () => {
  await page.context().close();
});

// Before sign-in the UI probes the session; 401 is the "no session" answer.
function expectSessionProbe() {
  watch.expectFailure(401, /\/_\/api\/admin\/session(\/s3-credentials)?$/, 'GET');
  watch.expectFailure(401, /\/_\/api\/whoami$/, 'GET');
}

async function signIn() {
  expectSessionProbe();
  await page.goto('/_/browse');
  await page.getByPlaceholder('Admin password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });
}

/** Lets a single step run on its own (`-g`): sign in when no step did yet. */
async function ensureSignedIn() {
  if (page.url() === 'about:blank') await signIn();
}

function bucketButton(name: string) {
  return page.getByRole('navigation', { name: 'Bucket list' }).getByRole('button', { name: new RegExp(`^${name}\\b`) });
}

async function openBucket(name: string) {
  await page.goto(`/_/browse/${name}/`);
  await expect(page.getByRole('navigation', { name: 'Breadcrumb' })).toContainText(name, { timeout: 30_000 });
}

function row(name: string) {
  return page.getByTestId(`object-row-${name}`);
}

async function createBucketViaUi(name: string) {
  await page.getByRole('navigation', { name: 'Bucket list' }).getByRole('button', { name: 'Create bucket' }).click();
  await page.getByRole('textbox', { name: 'Bucket name' }).fill(name);
  await page.getByRole('button', { name: 'Create', exact: true }).click();
  await expect(bucketButton(name)).toBeVisible({ timeout: 30_000 });
}

/** Upload files through the Upload page into `dest` (folder path or ''). */
async function uploadViaUi(
  bucket: string,
  dest: string,
  files: { name: string; mimeType: string; buffer: Buffer }[],
) {
  await openBucket(bucket);
  await page.getByRole('button', { name: 'Upload Files' }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`Upload to ${bucket}`) })).toBeVisible({ timeout: 30_000 });
  if (dest) {
    const input = page.getByRole('combobox', { name: /Destination path prefix/ });
    await input.fill(dest);
  }
  await page.locator('input[type=file]').first().setInputFiles(files);
  await expect(page.getByText('Upload complete')).toBeVisible({ timeout: 60_000 });
  await page.getByRole('button', { name: /Done — / }).click();
}

// ── 1. Login, bucket, upload, inspector, preview, download, delete ──────

const SMALL_TEXT = 'hello from the qa pass\nline two\n';
const TINY = 'ab'; // < 4 bytes
const FIVE = 'abcde'; // > 4 bytes
// Minimal valid ZIP: an empty archive is just the end-of-central-directory record.
const EMPTY_ZIP = Buffer.from([0x50, 0x4b, 0x05, 0x06, ...new Array(18).fill(0)]);

test('1. bootstrap login, create bucket, upload, inspect, preview, download, delete', async () => {
  await signIn();
  await createBucketViaUi(BUCKET);
  await bucketButton(BUCKET).click();

  await uploadViaUi(BUCKET, '', [
    { name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from(SMALL_TEXT) },
    { name: 'tiny.bin', mimeType: 'application/octet-stream', buffer: Buffer.from(TINY) },
    { name: 'five.txt', mimeType: 'text/plain', buffer: Buffer.from(FIVE) },
    { name: 'bundle.zip', mimeType: 'application/zip', buffer: EMPTY_ZIP },
  ]);
  await uploadViaUi(BUCKET, 'nested/deep/', [
    { name: 'inner.txt', mimeType: 'text/plain', buffer: Buffer.from('inner file\n') },
  ]);

  expect(await listKeys(BUCKET)).toEqual(
    ['bundle.zip', 'five.txt', 'nested/deep/inner.txt', 'notes.txt', 'tiny.bin'].sort(),
  );
  expect(await getText(BUCKET, 'tiny.bin')).toBe(TINY);
  expect(await getText(BUCKET, 'five.txt')).toBe(FIVE);

  // List in the UI.
  await openBucket(BUCKET);
  for (const n of ['notes.txt', 'tiny.bin', 'five.txt', 'bundle.zip']) {
    await expect(row(n)).toBeVisible();
  }
  await expect(page.getByRole('button', { name: 'nested/', exact: true })).toBeVisible();

  // Inspector.
  await row('notes.txt').click();
  await expect(page.getByRole('heading', { name: 'notes.txt' })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Object Info' })).toBeVisible();

  // Preview.
  await page.getByRole('button', { name: /Preview/ }).click();
  const preview = page.getByRole('dialog', { name: 'notes.txt', exact: true });
  await expect(preview).toContainText('hello from the qa pass');
  await page.keyboard.press('Escape');
  await expect(preview).toBeHidden();

  // Download and check the bytes.
  await page.getByRole('button', { name: /^download Download$/ }).click();
  await expect(page.getByText('File ready')).toBeVisible({ timeout: 30_000 });
  const [dl] = await Promise.all([
    page.waitForEvent('download'),
    page.getByRole('button', { name: /Save file/ }).click(),
  ]);
  expect(dl.suggestedFilename()).toBe('notes.txt');
  const path = await dl.path();
  const fs = await import('node:fs/promises');
  expect(await fs.readFile(path, 'utf8')).toBe(SMALL_TEXT);

  // Delete one object from the inspector.
  await page.getByRole('button', { name: 'Close inspector' }).click();
  await row('five.txt').click();
  await expect(page.getByRole('heading', { name: 'five.txt' })).toBeVisible();
  await page.getByRole('button', { name: /Delete object/ }).click();
  // It asks first; Cancel keeps the object.
  const confirm = page.getByRole('dialog').filter({ hasText: 'Delete permanently?' });
  await expect(confirm).toContainText('"five.txt"');
  await confirm.getByRole('button', { name: 'Cancel' }).click();
  await expect(confirm).toBeHidden();
  expect(await listKeys(BUCKET)).toContain('five.txt');
  await page.getByRole('button', { name: /Delete object/ }).click();
  await confirm.getByRole('button', { name: 'Delete' }).click();
  await expect(row('five.txt')).toBeHidden({ timeout: 30_000 });
  expect(await listKeys(BUCKET)).not.toContain('five.txt');
});

// ── 2. Folders: nested prefix, breadcrumbs, usage scan ──────────────────

function folderRow(name: string) {
  // The listing is a virtual table: rows are divs, not <tr>.
  return page.locator('.ant-table-row').filter({ has: page.getByRole('button', { name: `${name}/`, exact: true }) });
}

test('2. folders: navigate nested prefix, breadcrumbs, folder usage scan', async () => {
  await ensureSignedIn();
  await openBucket(BUCKET);
  await page.getByRole('button', { name: 'nested/', exact: true }).click();
  await page.getByRole('button', { name: 'deep/', exact: true }).click();
  await expect(row('inner.txt')).toBeVisible();
  const crumbs = page.getByRole('navigation', { name: 'Breadcrumb' });
  await expect(crumbs.locator('[aria-current="location"]')).toHaveText('deep');
  await crumbs.getByRole('link', { name: 'nested' }).click();
  await expect(crumbs.locator('[aria-current="location"]')).toHaveText('nested');
  await expect(page.getByRole('button', { name: 'deep/', exact: true })).toBeVisible();
  await crumbs.getByRole('link', { name: BUCKET }).click();
  await expect(crumbs.locator('[aria-current="location"]')).toHaveText(BUCKET);

  // Folder usage scan: "Size" on the folder row turns into the byte total.
  await folderRow('nested').getByRole('button', { name: /Size/ }).click();
  await expect(folderRow('nested')).toContainText('11 B', { timeout: 30_000 });
});

// ── 3. Bulk actions ─────────────────────────────────────────────────────

async function select(...names: string[]) {
  for (const n of names) await page.getByRole('checkbox', { name: `Select ${n}` }).check();
}

function toolbar() {
  return page.getByRole('toolbar', { name: 'Selection actions' });
}

/** Run Copy/Move from the selection bar into `bucket`/`dest`. */
async function bulkTo(op: 'Copy' | 'Move', bucket: string, dest: string) {
  await toolbar().getByRole('button', { name: new RegExp(`^${op} \\d+ selected`) }).click();
  const dlg = page.getByRole('dialog').filter({ hasText: 'Destination Bucket' });
  await expect(dlg).toBeVisible();
  const preview = dlg.getByText(/^Preview:/);
  if (!(await preview.textContent())?.includes(`${bucket}/`)) {
    await dlg.getByRole('combobox').fill(bucket);
    await page.locator(`.ant-select-dropdown .ant-select-item-option[title="${bucket}"]`).click();
  }
  await dlg.getByPlaceholder('/ (bucket root)').fill(dest);
  await dlg.locator('.ant-modal-footer .ant-btn-primary').click();
  return dlg;
}

function toast() {
  return page.locator('.ant-message-notice').last();
}

test('3. bulk copy, move, ZIP, delete; refusals for move-into-source and path escape', async () => {
  await ensureSignedIn();
  await s3().send(new CreateBucketCommand({ Bucket: BUCKET2 }));
  await s3().send(new PutObjectCommand({ Bucket: BUCKET, Key: 'mv/a.txt', Body: 'A' }));
  await s3().send(new PutObjectCommand({ Bucket: BUCKET, Key: 'mv/sub/a.txt', Body: 'SUB-A' }));
  await s3().send(new PutObjectCommand({ Bucket: BUCKET, Key: 'mv/b.txt', Body: 'B' }));

  // Copy two files to another bucket + prefix.
  await openBucket(BUCKET);
  await select('notes.txt', 'tiny.bin');
  await expect(toolbar()).toContainText('2 selected');
  await bulkTo('Copy', BUCKET2, 'copied/');
  await expect(toast()).toContainText('2 items copied', { timeout: 30_000 });
  expect(await listKeys(BUCKET2)).toEqual(['copied/notes.txt', 'copied/tiny.bin']);
  expect(await getText(BUCKET2, 'copied/notes.txt')).toBe(SMALL_TEXT);

  // Move a file and a folder to a prefix in the same bucket.
  await openBucket(BUCKET);
  await select('tiny.bin', 'nested/');
  await bulkTo('Move', BUCKET, 'moved/');
  await expect(toast()).toContainText('moved', { timeout: 30_000 });
  const afterMove = await listKeys(BUCKET);
  expect(afterMove).toContain('moved/tiny.bin');
  expect(afterMove).toContain('moved/nested/deep/inner.txt');
  expect(afterMove).not.toContain('tiny.bin');
  expect(afterMove).not.toContain('nested/deep/inner.txt');

  // Move INTO a subfolder of a selected source: mv/a.txt would land on
  // mv/sub/a.txt, which is itself selected (inside mv/sub/). Server: 409.
  await openBucket(BUCKET);
  await page.getByRole('button', { name: 'mv/', exact: true }).click();
  await select('a.txt', 'sub/');
  watch.expectFailure(409, /\/_\/api\/admin\/objects\/move$/, 'POST');
  await bulkTo('Move', BUCKET, 'mv/sub/');
  await expect(page.locator('.ant-message-notice-error')).toContainText(/selected source/, { timeout: 30_000 });
  expect(await getText(BUCKET, 'mv/sub/a.txt')).toBe('SUB-A');
  expect(await getText(BUCKET, 'mv/a.txt')).toBe('A');
  // The selection survives a refused move, so the user can retry.
  await expect(toolbar()).toContainText('2 selected');

  // Path escape in the destination is refused in the picker, before any request.
  await toolbar().getByRole('button', { name: /^Copy \d+ selected/ }).click();
  const picker = page.getByRole('dialog').filter({ hasText: 'Destination Bucket' });
  await picker.getByPlaceholder('/ (bucket root)').fill('../escape/');
  await expect(picker.getByRole('alert')).toContainText('cannot be "." or ".."');
  await expect(picker.locator('.ant-modal-footer .ant-btn-primary')).toBeDisabled();
  await picker.getByRole('button', { name: 'Cancel' }).click();
  // The server refuses the same shape on its own (a client that skips the UI).
  const r = await page.request.post('/_/api/admin/objects/copy', {
    data: { source_bucket: BUCKET, dest_bucket: BUCKET, dest_prefix: '../escape/', items: [{ source_key: 'mv/a.txt', relative: 'a.txt' }] },
  });
  expect(r.status()).toBe(400);
  expect(await r.json()).toEqual({
    error: 'invalid_path',
    message: `dest_prefix: invalid path "../escape/": '.' and '..' segments are not allowed`,
  });
  expect((await listKeys(BUCKET)).some((k) => k.includes('escape'))).toBe(false);

  // Upload to a '..' destination: refused on the page, no request is sent.
  await page.goto(`/_/browse/${BUCKET}/`);
  await page.getByRole('button', { name: 'Upload Files' }).click();
  await page.getByRole('combobox', { name: /Destination path prefix/ }).fill('../up-escape/');
  await expect(page.getByRole('alert').filter({ hasText: 'cannot be "." or ".."' })).toBeVisible();
  await expect(page.getByRole('button', { name: /Select files/ })).toBeDisabled();
  await page.locator('input[type=file]').first().setInputFiles({ name: 'esc.txt', mimeType: 'text/plain', buffer: Buffer.from('x') });
  await expect(page.getByRole('list', { name: 'Upload queue' })).toHaveCount(0);
  // "New folder" refuses '..' too.
  await page.getByRole('combobox', { name: /Destination path prefix/ }).fill('');
  await page.getByRole('button', { name: /New folder/ }).click();
  const folderDlg = page.getByRole('dialog', { name: 'Create folder' });
  await folderDlg.getByRole('textbox').fill('..');
  await expect(folderDlg.getByRole('alert')).toBeVisible();
  await expect(folderDlg.getByRole('button', { name: 'Create' })).toBeDisabled();
  await folderDlg.getByRole('button', { name: 'Cancel' }).click();

  // ZIP download of the selection.
  await openBucket(BUCKET);
  await page.getByRole('button', { name: 'mv/', exact: true }).click();
  await select('a.txt', 'b.txt');
  const [zip] = await Promise.all([
    page.waitForEvent('download'),
    toolbar().getByRole('button', { name: /as ZIP/ }).click(),
  ]);
  const fs = await import('node:fs/promises');
  const bytes = await fs.readFile(await zip.path());
  expect(bytes.subarray(0, 2).toString()).toBe('PK');
  expect(bytes.includes(Buffer.from('a.txt'))).toBe(true);
  expect(bytes.includes(Buffer.from('b.txt'))).toBe(true);

  // Bulk delete (confirm dialog).
  await toolbar().getByRole('button', { name: /^Delete \d+ selected/ }).click();
  await page.getByRole('dialog').filter({ hasText: 'Delete permanently?' }).getByRole('button', { name: 'Delete' }).click();
  await expect(row('a.txt')).toBeHidden({ timeout: 30_000 });
  await expect(row('b.txt')).toBeHidden();
  const left = await listKeys(BUCKET, 'mv/');
  expect(left).toEqual(['mv/sub/a.txt']);
});

// ── 4. Every admin page, command palette, shortcuts help ───────────────

/** Every leaf of ADMIN_IA, read from the source so a new page is covered. */
function adminLeaves(): { path: string; label: string }[] {
  const src = readFileSync(new URL('../src/components/adminNavigation.tsx', import.meta.url), 'utf8');
  return [...src.matchAll(/path: '([^']+)',\s*label: '([^']+)'/g)].map((m) => ({ path: m[1], label: m[2] }));
}

function escapeRe(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** The page is quiet: no request for 500 ms (the logs page's SSE stream excepted). */
async function settle() {
  await page.waitForLoadState('networkidle', { timeout: 5_000 }).catch(() => undefined);
}

async function openAdmin(path: string, label: string) {
  await page.goto(`/_/admin/${path}`);
  await expect(page.getByRole('heading', { level: 2, name: new RegExp(`^${escapeRe(label)}\\b`) })).toBeVisible({ timeout: 30_000 });
  await settle();
}

test('4. every admin page loads cleanly; Ctrl+K navigates; ? opens shortcuts help', async () => {
  await ensureSignedIn();
  const leaves = adminLeaves();
  expect(leaves.length).toBeGreaterThanOrEqual(17);
  for (const { path, label } of leaves) {
    await openAdmin(path, label);
    // Surface the page in the failure message, not only the URL.
    watch.assertClean(`admin page ${path}`);
  }

  // Command palette.
  await openAdmin('dashboard', 'Dashboard');
  await page.keyboard.press('Control+K');
  const palette = page.getByPlaceholder('Type to filter pages or actions...');
  await expect(palette).toBeVisible();
  await palette.fill('Audit log');
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/\/_\/admin\/diagnostics\/audit$/);
  await expect(page.getByRole('heading', { level: 2, name: /^Audit log/ })).toBeVisible();

  // Shortcuts help.
  await page.locator('body').click({ position: { x: 5, y: 5 } }).catch(() => undefined);
  await page.keyboard.press('?');
  const help = page.getByRole('dialog', { name: 'Keyboard shortcuts' });
  await expect(help).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(help).toBeHidden();
});

// ── 5. IAM: scoped user, group, delete last user → bootstrap ──────────

const SCOPED_KEY = `qa-scoped-${RUN}`;
const SCOPED_SECRET = 'qa-scoped-secret-0123456789abcdef';

function s3As(accessKeyId: string, secretAccessKey: string): S3Client {
  return new S3Client({
    endpoint: BASE,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId, secretAccessKey },
  });
}

async function putStatus(client: S3Client, bucket: string, key: string): Promise<number> {
  try {
    await client.send(new PutObjectCommand({ Bucket: bucket, Key: key, Body: 'x' }));
    return 200;
  } catch (e) {
    return (e as { $metadata?: { httpStatusCode?: number } }).$metadata?.httpStatusCode ?? -1;
  }
}

test('5. IAM: create prefix-scoped user and a group, delete the last user → bootstrap mode', async () => {
  await ensureSignedIn();
  await s3().send(new CreateBucketCommand({ Bucket: BUCKET })).catch(() => undefined);
  await openAdmin('access/users', 'Users');
  await page.getByRole('button', { name: /New User/ }).click();
  await page.getByRole('textbox', { name: 'User name' }).fill(`qa-scoped-${RUN}`);
  await page.getByPlaceholder('e.g. user@company.com').fill(SCOPED_KEY);
  await page.getByPlaceholder('e.g. mysecretkey or leave empty').fill(SCOPED_SECRET);
  const resource = page.getByRole('combobox', { name: 'my-bucket/builds/*' });
  await resource.fill(`${BUCKET}/scoped/*`);
  // Leaving the WHERE field must not move CAN DO: its suggestion chips used
  // to collapse on blur and shift the action chips up under the pointer.
  const write = page.getByRole('checkbox', { name: 'Write' });
  const before = await write.boundingBox();
  await resource.blur();
  await page.waitForTimeout(300); // longer than the field's 150 ms blur timer
  expect(await write.boundingBox()).toEqual(before);
  await resource.focus();
  // Click straight from the focused field, as a user does.
  await write.check();
  await page.getByRole('button', { name: 'Create user' }).click();
  const userItem = page.getByText(`qa-scoped-${RUN}`, { exact: true }).first();
  await expect(userItem).toBeVisible({ timeout: 30_000 });

  // The proxy is now in IAM mode; the scoped key may write only under scoped/.
  const scoped = s3As(SCOPED_KEY, SCOPED_SECRET);
  await expect.poll(() => putStatus(scoped, BUCKET, 'scoped/ok.txt'), { timeout: 15_000 }).toBe(200);
  expect(await putStatus(scoped, BUCKET, 'outside.txt')).toBe(403);
  const who = await (await page.request.get('/_/api/whoami')).json();
  expect(who.mode).toBe('iam');

  // A group.
  await openAdmin('access/groups', 'Groups');
  await page.getByRole('button', { name: /New Group/ }).click();
  await page.getByRole('textbox', { name: 'Group name' }).fill(`qa-group-${RUN}`);
  await page.getByRole('button', { name: 'Create group' }).click();
  await expect(page.getByText(`qa-group-${RUN}`, { exact: true }).first()).toBeVisible({ timeout: 30_000 });

  // The first IAM user migrates the bootstrap key into a `legacy-admin` user,
  // so the bootstrap key keeps working in IAM mode.
  await openAdmin('access/users', 'Users');
  await expect(page.getByText('legacy-admin', { exact: true })).toBeVisible();
  expect(await putStatus(s3(), BUCKET, 'legacy-key-in-iam.txt')).toBe(200);

  // Delete the scoped user, then the last one (legacy-admin).
  for (const name of [`qa-scoped-${RUN}`, 'legacy-admin']) {
    await page.getByText(name, { exact: true }).first().click();
    await expect(page.getByRole('textbox', { name: 'User name' })).toHaveValue(name);
    page.once('dialog', (d) => d.accept());
    await page.getByRole('button', { name: 'Delete User' }).click();
    await expect(page.getByText(name, { exact: true })).toHaveCount(0, { timeout: 30_000 });
  }
  await expect(page.getByText('No users yet')).toBeVisible({ timeout: 30_000 });

  // Back to bootstrap mode, NOT open access: unsigned S3 is still refused,
  // the bootstrap key works again, and the deleted key does not.
  await expect.poll(async () => (await (await page.request.get('/_/api/whoami')).json()).mode, { timeout: 15_000 }).toBe('bootstrap');
  const unsigned = await page.request.get(`/${BUCKET}/`);
  expect(unsigned.status()).toBe(403);
  const unsignedPut = await page.request.put(`/${BUCKET}/anon.txt`, { data: 'x' });
  expect(unsignedPut.status()).toBe(403);
  await expect.poll(() => putStatus(s3(), BUCKET, 'bootstrap-again.txt'), { timeout: 15_000 }).toBe(200);
  expect(await putStatus(scoped, BUCKET, 'scoped/after-delete.txt')).toBe(403);

  // The browser still works for the bootstrap admin.
  await openBucket(BUCKET);
  await expect(row('bootstrap-again.txt')).toBeVisible();
});

// ── 6. Config: export, section edit + apply, YAML import env rules ─────

async function accountMenu(item: string) {
  await page.getByRole('button', { name: /Account menu/ }).click();
  await page.getByRole('menuitem', { name: item }).click();
}

function yamlModal(kind: 'Export' | 'Import') {
  return page.getByRole('dialog').filter({ hasText: kind === 'Export' ? 'Export configuration as YAML' : 'Paste a full YAML config document' });
}

async function exportYaml(): Promise<string> {
  await accountMenu('Export settings YAML');
  const dlg = yamlModal('Export');
  const text = dlg.locator('textarea');
  await expect(text).toHaveValue(/storage:/, { timeout: 30_000 });
  const yaml = await text.inputValue();
  await dlg.getByRole('button', { name: 'Close', exact: true }).last().click();
  await expect(dlg).toBeHidden();
  return yaml;
}

/** Paste `yaml` into the import dialog and press Validate. */
async function importYaml(yaml: string) {
  await accountMenu('Import settings YAML');
  const dlg = yamlModal('Import');
  await dlg.locator('textarea').fill(yaml);
  await dlg.getByRole('button', { name: 'Validate' }).click();
  return dlg;
}

test('6. config: export YAML, edit + Review & apply, persisted; env refs in YAML import', async () => {
  await ensureSignedIn();
  await openAdmin('system', 'System');
  // The Logging card shows the level the process runs at. When RUST_LOG
  // sets it (it beats the file), the field is read-only with the variable
  // named; it used to show the file's default ("Debug") instead.
  const cfg = await (await page.request.get('/_/api/admin/config')).json();
  const rustLog = (cfg.env_overrides ?? []).find((o: { env: string }) => o.env === 'RUST_LOG');
  const logField = page.locator('.dg-field').filter({ hasText: 'advanced.log_level' });
  if (rustLog) {
    await expect(logField).toContainText('RUST_LOG');
    await expect(logField).toContainText(rustLog.value);
  }
  const exported = await exportYaml();
  expect(exported).toContain(PUBLIC_BUCKET);
  expect(exported).not.toContain(SECRET_KEY);

  // Edit a hot-reloadable field and apply it through the ApplyDialog.
  const mdCache = page.locator('.dg-field').filter({ hasText: 'advanced.metadata_cache_mb' }).getByRole('spinbutton');
  await expect(mdCache).toBeVisible();
  const newValue = String(60 + (Date.now() % 30));
  await mdCache.fill(newValue);
  await mdCache.blur();
  await page.getByRole('button', { name: 'Review & apply' }).click();
  const apply = page.getByRole('dialog').filter({ hasText: 'Review changes before applying' });
  await expect(apply).toContainText('metadata_cache_mb');
  await apply.getByRole('button', { name: 'Apply and persist changes' }).click();
  await expect(apply).toBeHidden({ timeout: 30_000 });

  await page.reload();
  await expect(page.getByRole('heading', { level: 2, name: /^System/ })).toBeVisible({ timeout: 30_000 });
  await expect(mdCache).toHaveValue(newValue);
  const edited = await exportYaml();
  expect(edited).toContain(`metadata_cache_mb: ${newValue}`);

  // YAML import: an env ref the boot config does not use is refused, with a
  // message that names it and says why.
  const withLevel = (lvl: string) =>
    edited.replace(/^  log_level: .*\n/m, '').replace(/advanced:\n/, `advanced:\n  log_level: "${lvl}"\n`);
  watch.expectFailure(400, /\/_\/api\/admin\/config\/validate$/, 'POST');
  let dlg = await importYaml(withLevel('${env:NOT_IN_BOOT_CONFIG}'));
  const err = dlg.getByRole('alert').filter({ hasText: 'Validation error' });
  await expect(err).toBeVisible({ timeout: 30_000 });
  await expect(err).toContainText('NOT_IN_BOOT_CONFIG');
  await expect(dlg.getByRole('button', { name: 'Apply and Persist' })).toBeDisabled();
  await dlg.getByRole('button', { name: 'Close', exact: true }).last().click();

  // With a `:-default` the same ref resolves to the default and applies.
  // (Not the RUST_LOG value of the local run: a value equal to the env value
  // is an echo and never reaches the file.)
  dlg = await importYaml(withLevel('${env:QA_UNSET_LEVEL:-deltaglider_proxy=warn}'));
  await expect(dlg.getByRole('alert').filter({ hasText: 'YAML is valid' })).toBeVisible({ timeout: 30_000 });
  // A successful apply reloads the admin page so every panel re-fetches.
  await Promise.all([
    page.waitForEvent('load'),
    dlg.getByRole('button', { name: 'Apply and Persist' }).click(),
  ]);
  await expect(page.getByRole('heading', { level: 2, name: /^System/ })).toBeVisible({ timeout: 30_000 });
  const after = await exportYaml();
  expect(after).toContain('deltaglider_proxy=warn');
  // The imported document carried the section edit from above.
  expect(after).toContain(`metadata_cache_mb: ${newValue}`);
});

// ── 7. Backup, audit log, jobs ──────────────────────────────────────────

test('7. backup export downloads a zip; audit log shows actors; jobs empty state', async () => {
  await ensureSignedIn();
  await openAdmin('system', 'System');
  const [dl] = await Promise.all([
    page.waitForEvent('download'),
    page.getByRole('button', { name: /Download backup/ }).click(),
  ]);
  expect(dl.suggestedFilename()).toMatch(/^dgp-backup-.*\.zip$/);
  const fs = await import('node:fs/promises');
  const zip = await fs.readFile(await dl.path());
  expect(zip.subarray(0, 2).toString()).toBe('PK');

  await openAdmin('diagnostics/audit', 'Audit log');
  const table = page.locator('#main-content');
  // Sign-ins record the bootstrap principal; admin changes made with the
  // bootstrap session record `admin` (the break-glass label).
  await expect(table).toContainText('login', { timeout: 30_000 });
  await expect(table).toContainText('bootstrap');
  await expect(table).toContainText('export_backup');

  await openAdmin('jobs', 'Jobs');
  await expect(page.getByText(/No jobs yet/)).toBeVisible({ timeout: 30_000 });
});

// ── 8. Security behaviours the UI must survive ──────────────────────────

test('8. cross-origin admin POST refused; HTML served sandboxed; anonymous content-type override refused', async () => {
  await ensureSignedIn();
  // Cross-origin POST with the session cookie: refused before the handler.
  const evil = await page.request.post('/_/api/admin/objects/delete', {
    headers: { Origin: 'https://evil.example' },
    data: { bucket: BUCKET, keys: ['notes.txt'] },
  });
  expect(evil.status()).toBe(403);
  expect(await evil.text()).toContain('cross_origin_request');
  const evil2 = await page.request.post('/_/api/admin/objects/delete', {
    headers: { 'Sec-Fetch-Site': 'cross-site' },
    data: { bucket: BUCKET, keys: ['notes.txt'] },
  });
  expect(evil2.status()).toBe(403);
  expect(await listKeys(BUCKET)).toContain('notes.txt');

  // The UI's own write still works: delete notes.txt through the selection bar.
  await openBucket(BUCKET);
  await select('notes.txt');
  await toolbar().getByRole('button', { name: /^Delete \d+ selected/ }).click();
  await page.getByRole('dialog').filter({ hasText: 'Delete permanently?' }).getByRole('button', { name: 'Delete' }).click();
  await expect(row('notes.txt')).toBeHidden({ timeout: 30_000 });
  expect(await listKeys(BUCKET)).not.toContain('notes.txt');

  // An uploaded .html object is served with a sandbox CSP (signed GET).
  const html = '<html><body><script>document.title="pwned"</script>hi</body></html>';
  await s3().send(new PutObjectCommand({ Bucket: BUCKET, Key: 'page.html', Body: html, ContentType: 'text/html' }));
  const signed = await getSignedUrl(s3(), new GetObjectCommand({ Bucket: BUCKET, Key: 'page.html' }), { expiresIn: 300 });
  const got = await page.request.get(signed);
  expect(got.status()).toBe(200);
  expect(got.headers()['content-security-policy'] ?? '').toContain('sandbox');

  // Public prefix: anonymous read works and is sandboxed, but an anonymous
  // response-content-type override is refused.
  await s3().send(new CreateBucketCommand({ Bucket: PUBLIC_BUCKET })).catch(() => undefined);
  await s3().send(new PutObjectCommand({ Bucket: PUBLIC_BUCKET, Key: 'pub/page.html', Body: html, ContentType: 'text/html' }));
  const anon = await page.request.get(`/${PUBLIC_BUCKET}/pub/page.html`);
  expect(anon.status()).toBe(200);
  expect(anon.headers()['content-security-policy'] ?? '').toContain('sandbox');
  const override = await page.request.get(`/${PUBLIC_BUCKET}/pub/page.html?response-content-type=text%2Fhtml`);
  expect(override.status()).toBe(400);
  // Outside the public prefix, anonymous reads are refused.
  const priv = await page.request.get(`/${PUBLIC_BUCKET}/private.txt`);
  expect(priv.status()).toBe(403);
});

// ── 9. Logout, log back in, state persists ──────────────────────────────

test('9. sign out, sign back in, data and settings persist', async () => {
  await ensureSignedIn();
  await openBucket(BUCKET);
  page.once('dialog', (d) => d.accept());
  await page.getByRole('button', { name: /Account menu/ }).click();
  await page.getByRole('menuitem', { name: 'Sign out' }).click();
  await expect(page.getByPlaceholder('Admin password')).toBeVisible({ timeout: 30_000 });
  // The session is gone on the server, not only in the page.
  watch.expectFailure(401, /\/_\/api\/admin\/users$/, 'GET');
  const probe = await page.request.get('/_/api/admin/users');
  expect(probe.status()).toBe(401);

  expectSessionProbe();
  await page.getByPlaceholder('Admin password').fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Bucket list' })).toBeVisible({ timeout: 30_000 });
  await openBucket(BUCKET);
  await expect(row('bundle.zip')).toBeVisible();
  await openAdmin('system', 'System');
  expect(await exportYaml()).toContain('deltaglider_proxy=warn');
});
