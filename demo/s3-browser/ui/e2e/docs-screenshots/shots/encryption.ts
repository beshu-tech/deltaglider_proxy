/**
 * Encryption shots: the per-backend encryption editor on the Backends page
 * (docs: how-to/encrypt-data-at-rest, how-to/rotate-encryption-keys).
 *
 * Later shots of the same run (audit log, jobs, browser) must not see a
 * changed proxy, and each shot runs twice (light, dark). So no shot here
 * changes server state: an Apply goes to a stubbed PUT, and a state that
 * only an apply reaches (a backend already in AES mode, an active legacy
 * shim) comes from a patched copy of the real GET response.
 */
import type { Page, Route } from '@playwright/test';
import type { Shot, Target } from '../shot';
import { nav } from './common';

type Json = Record<string, unknown>;

/** Match an admin API path exactly (query ignored, `%3A` decoded). */
const path = (p: string) => (url: URL) => decodeURIComponent(url.pathname) === p;

/** Serve `patch(real body)` for `method` requests to `p`; other methods pass through. */
export async function patchJson(page: Page, p: string, method: string, patch: (body: Json) => Json): Promise<void> {
  await page.route(path(p), async (route: Route) => {
    if (route.request().method() !== method) return route.fallback();
    const res = await route.fetch();
    await route.fulfill({ response: res, json: patch((await res.json()) as Json) });
  });
}

/** Answer `method` requests to `p` with `body`, without reaching the proxy. */
export async function fakeJson(page: Page, p: string, method: string, body: unknown, status = 200): Promise<void> {
  await page.route(path(p), async (route: Route) => {
    if (route.request().method() !== method) return route.fallback();
    await route.fulfill({ status, json: body });
  });
}

/** Seconds since the epoch at the fixed capture instant, minus `ago` seconds. */
export const at = (ago: number) => Math.floor(Date.parse(process.env.DOCS_NOW ?? new Date().toISOString()) / 1000) - ago;

const BACKENDS = '/_/api/admin/backends';
const STORAGE = '/_/api/admin/config/section/storage';

/** The card of one backend on the Backends page (the innermost block with its name and its encryption editor). */
const card = (name: string): Target => ({
  css: `div:has(> div :text-is("${name}")):has([data-testid="encryption-mode-select"])`,
  nth: -1,
});
const cardLoc = (page: Page, name: string) =>
  page.locator(`div:has(> div :text-is("${name}")):has([data-testid="encryption-mode-select"])`).last();

/** The apply of an encryption change succeeds without reaching the proxy. */
async function stubStoragePut(page: Page): Promise<void> {
  await fakeJson(page, STORAGE, 'PUT', { ok: true, warnings: [], requires_restart: false });
}

/** GET /backends reports hetzner-fsn1 in proxy-AES mode (optionally with an active legacy shim). */
async function hetznerIsAes(page: Page, shim: boolean): Promise<void> {
  await patchJson(page, BACKENDS, 'GET', (body) => {
    const list = body.backends as { name: string; encryption: Json }[];
    for (const b of list) {
      if (b.name === 'hetzner-fsn1') {
        b.encryption = { mode: 'aes256-gcm-proxy', has_key: true, key_id: shim ? '3f1a9c0e5b7d2468' : 'hetzner-2026-06', shim_active: shim };
      }
    }
    return body;
  });
  if (shim) {
    await fakeJson(page, `${BACKENDS}/hetzner-fsn1/legacy-key-usage`, 'GET', {
      backend: 'hetzner-fsn1',
      legacy_key_id: 'hetzner-2026-06',
      buckets: ['downloads', 'releases'],
      objects_scanned: 7,
      objects_under_legacy_key: 0,
      references_scanned: 2,
      references_under_legacy_key: 0,
      examples: [],
      errors: [],
      complete: true,
      limit: 10000,
      safe_to_clear: true,
    });
  }
  await page.reload();
  await cardLoc(page, 'hetzner-fsn1').waitFor();
}

async function pickMode(page: Page, backend: string, label: string): Promise<void> {
  const c = cardLoc(page, backend);
  await c.scrollIntoViewIfNeeded();
  await c.getByRole('combobox', { name: 'Encryption mode' }).click();
  await page.locator(`.ant-select-dropdown:visible .ant-select-item-option[title="${label}"]`).click();
}

/** Mode AES-256-GCM on hetzner-fsn1: the browser generates a key; tick the "stored safely" box. */
async function aesKeyReady(page: Page): Promise<void> {
  await pickMode(page, 'hetzner-fsn1', 'AES-256-GCM (proxy-side)');
  await page.getByTestId('encryption-key-stored').check();
  // Always the same scroll position: scrollIntoViewIfNeeded depended on
  // where check() had scrolled to, so the shot moved from run to run.
  await page.getByTestId('encryption-apply').evaluate((el) => el.scrollIntoView({ block: 'center' }));
  await page.mouse.move(1, 1);
}

/** The generated key is random: paint over it so the images stay stable. */
const KEY_MASK: Target = { css: 'textarea[readonly]' };

export const ENCRYPTION_SHOTS: Shot[] = [
  {
    id: 'encrypt-mode-select',
    route: '/_/admin/storage/backends',
    alt: 'The Backends page with the Encryption mode list of hetzner-fsn1 open; callout 1 marks Backends in the sidebar, callout 2 marks the Encryption mode list, and callout 3 marks the AES-256-GCM (proxy-side) option.',
    setup: async (page) => {
      const c = cardLoc(page, 'hetzner-fsn1');
      await c.scrollIntoViewIfNeeded();
      await c.getByRole('combobox', { name: 'Encryption mode' }).click();
      await page.locator('.ant-select-dropdown:visible .ant-select-item-option[title="AES-256-GCM (proxy-side)"]').waitFor();
    },
    annotations: [
      { target: nav('Backends'), kind: 'callout', label: '1', side: 'right' },
      { target: { testId: 'encryption-mode-select', within: card('hetzner-fsn1') }, kind: 'callout', label: '2', side: 'left' },
      { target: { css: '.ant-select-dropdown:visible .ant-select-item-option[title="AES-256-GCM (proxy-side)"]' }, kind: 'box', label: '3' },
    ],
  },
  {
    id: 'encrypt-key',
    route: '/_/admin/storage/backends',
    alt: 'The encryption editor of hetzner-fsn1 holds a generated key, which is hidden here; callout 4 marks Copy to clipboard, callout 5 marks the I have stored this key safely checkbox, and callout 6 marks Apply.',
    setup: aesKeyReady,
    mask: [KEY_MASK],
    annotations: [
      { target: { role: 'button', name: 'Copy to clipboard' }, kind: 'callout', label: '4', side: 'left' },
      { target: { testId: 'encryption-key-stored' }, kind: 'callout', label: '5', side: 'left' },
      { target: { testId: 'encryption-apply' }, kind: 'callout', label: '6', side: 'right' },
    ],
  },
  {
    id: 'encrypt-reencrypt-proposal',
    route: '/_/admin/storage/backends',
    alt: 'After the apply, the dialog Encrypt existing objects? lists the buckets of hetzner-fsn1; the arrow points at Start now.',
    setup: async (page) => {
      await stubStoragePut(page);
      await aesKeyReady(page);
      await page.getByTestId('encryption-apply').click();
      await page.getByRole('button', { name: /^Start now/ }).waitFor();
    },
    annotations: [{ target: { role: 'button', name: /^Start now/ }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'encrypt-kms',
    route: '/_/admin/storage/backends',
    alt: 'The encryption editor of aws-dr in SSE-KMS mode; callout 1 marks the KMS key ARN or alias field and callout 2 marks the S3 bucket keys checkbox.',
    setup: async (page) => {
      await pickMode(page, 'aws-dr', 'SSE-KMS (AWS KMS)');
      await page.getByPlaceholder('arn:aws:kms:us-east-1:123456789012:key/abcd-efgh').fill('arn:aws:kms:eu-west-1:123456789012:key/abcd-ef01');
      await page.getByTestId('encryption-apply').evaluate((el) => el.scrollIntoView({ block: 'center' }));
      await page.mouse.move(1, 1);
    },
    annotations: [
      { target: { placeholder: 'arn:aws:kms:us-east-1:123456789012:key/abcd-efgh' }, kind: 'box', label: '1' },
      { target: { text: /^Enable S3 bucket keys/ }, kind: 'callout', label: '2', side: 'right' },
    ],
  },
  {
    id: 'rotate-key-button',
    route: '/_/admin/storage/backends',
    alt: 'The hetzner-fsn1 backend uses AES-256-GCM with the key id hetzner-2026-06; the arrow points at the Rotate key button.',
    setup: async (page) => {
      await hetznerIsAes(page, false);
      await cardLoc(page, 'hetzner-fsn1').scrollIntoViewIfNeeded();
    },
    annotations: [{ target: { role: 'button', name: 'Rotate key', within: card('hetzner-fsn1') }, kind: 'arrow', side: 'left' }],
  },
  {
    id: 'rotate-reencrypt',
    route: '/_/admin/storage/backends',
    alt: 'After the rotation, the dialog Re-encrypt existing objects with the new key? lists the buckets of hetzner-fsn1; the arrow points at Start now.',
    setup: async (page) => {
      await hetznerIsAes(page, false);
      await stubStoragePut(page);
      await cardLoc(page, 'hetzner-fsn1').getByRole('button', { name: 'Rotate key' }).click();
      await page.getByTestId('encryption-key-stored').check();
      await page.getByTestId('encryption-apply').click();
      await page.getByRole('button', { name: /^Start now/ }).waitFor();
    },
    annotations: [{ target: { role: 'button', name: /^Start now/ }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'rotate-clear-legacy',
    route: '/_/admin/storage/backends',
    alt: 'The hetzner-fsn1 backend shows the legacy key banner, which reports that no object uses the legacy key id hetzner-2026-06; a box marks Clear legacy key.',
    setup: async (page) => {
      await hetznerIsAes(page, true);
      const btn = page.getByRole('button', { name: 'Clear legacy key' });
      await btn.waitFor();
      await btn.scrollIntoViewIfNeeded();
    },
    // A box: an arrow from the side crosses the banner text.
    annotations: [{ target: { role: 'button', name: 'Clear legacy key' }, kind: 'box' }],
  },
];

// ── how-to/encrypt-one-bucket-on-a-shared-backend (issue #116 P2) ──
//
// The seed keeps db-archive on local-disk; the page shows the example of
// the how-to (db-archive on hetzner-fsn1 next to plaintext downloads) from a
// patched GET /buckets. No shot changes server state: validate and PUT are
// stubbed, and the answer after an apply comes from the same patch.

const ORIGINS = '/_/api/admin/buckets';
const VALIDATE = '/_/api/admin/config/section/storage/validate';

/** The server's per-bucket answer for the how-to's example. */
function effective(own: boolean): Json {
  return {
    mode: own ? 'aes256-gcm-proxy' : 'none',
    source: own ? 'bucket' : 'backend',
    backend: 'hetzner-fsn1',
    has_key: own,
    ...(own ? { key_id: 'db-archive-2026-10' } : {}),
    encrypts: own,
    reencrypt: own,
    pinned: true,
    bucket_modes: ['none', 'aes256-gcm-proxy'],
    inherited: { mode: 'none', encrypts: false },
  };
}

/** GET /buckets: db-archive on hetzner-fsn1, plaintext, until `applied()` says
 *  that the shot's (stubbed) apply ran; then with its own key. */
async function exampleOrigins(page: Page, applied: () => boolean = () => false): Promise<void> {
  await page.route((url) => decodeURIComponent(url.pathname) === ORIGINS, async (route) => {
    if (route.request().method() !== 'GET') return route.fallback();
    const res = await route.fetch();
    const body = (await res.json()) as { buckets: Json[] };
    for (const b of body.buckets) {
      if (b.name === 'db-archive') {
        b.backend_name = 'hetzner-fsn1';
        b.effective_encryption = effective(applied());
      } else if (b.name === 'downloads') {
        b.effective_encryption = { ...effective(false) };
      }
    }
    await route.fulfill({ response: res, json: body });
  });
  await page.reload();
}

async function openDbArchive(page: Page): Promise<void> {
  await page.getByRole('button', { name: /^db-archive — click to edit/ }).click();
  await page.getByTestId('bucket-encryption-group').waitFor();
}

/** Bucket key chosen, the key id fixed, and the last 6 characters typed. */
async function bucketKeyReady(page: Page): Promise<void> {
  await openDbArchive(page);
  await page.getByText('Encrypt with a bucket key', { exact: true }).click();
  await page.getByLabel('Key id').fill('db-archive-2026-10');
  const key = await page.getByLabel('Generated key').inputValue();
  await page.getByLabel('Last 6 characters of the key').fill(key.slice(-6));
  await page.getByRole('button', { name: 'Review & apply' }).evaluate((el) => el.scrollIntoView({ block: 'center' }));
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.mouse.move(1, 1);
}

export const BUCKET_ENCRYPTION_SHOTS: Shot[] = [
  {
    id: 'bucket-encryption-choose',
    route: '/_/admin/storage/buckets',
    alt: 'The open db-archive row on the Buckets page; callout 1 marks the db-archive row, and callout 2 marks the Encrypt with a bucket key option in the Encryption group.',
    setup: async (page) => {
      await exampleOrigins(page);
      await openDbArchive(page);
      await page.getByTestId('bucket-encryption-group').evaluate((el) => el.scrollIntoView({ block: 'center' }));
      await page.mouse.move(1, 1);
    },
    // The seed routes db-archive to local-disk: crop to the row header
    // (its route chip hidden) and the Encryption group.
    clip: { union: [{ role: 'button', name: /^db-archive — click to collapse/ }, { testId: 'bucket-encryption-group' }] },
    clipPadding: 48,
    mask: [{ text: /LOCAL-DISK/i }],
    annotations: [
      { target: { role: 'button', name: /^db-archive — click to collapse/ }, kind: 'callout', label: '1', side: 'left' },
      { target: { text: 'Encrypt with a bucket key', exact: true }, kind: 'callout', label: '2', side: 'right' },
    ],
  },
  {
    id: 'bucket-encryption-key',
    route: '/_/admin/storage/buckets',
    alt: 'The key setup of db-archive with the generated key hidden; callout 5 marks the Key id field, a box marks Download key file (step 6), callout 7 marks the field for the last 6 characters, and callout 8 marks Review & apply.',
    setup: async (page) => {
      await exampleOrigins(page);
      await bucketKeyReady(page);
    },
    viewport: { width: 1040, height: 980 },
    mask: [KEY_MASK, { label: 'Last 6 characters of the key' }],
    clip: { testId: 'bucket-encryption-group' },
    clipPadding: 32,
    annotations: [
      { target: { label: 'Key id' }, kind: 'callout', label: '5', side: 'right' },
      { target: { role: 'button', name: /Download key file/ }, kind: 'box' },
      { target: { label: 'Last 6 characters of the key' }, kind: 'callout', label: '7', side: 'right' },
      { target: { role: 'button', name: 'Review & apply' }, kind: 'callout', label: '8', side: 'right' },
    ],
  },
  {
    id: 'bucket-encryption-reencrypt',
    route: '/_/admin/storage/buckets',
    alt: 'The dialog Encrypt existing objects? lists only db-archive; the arrow points at Start now.',
    setup: async (page) => {
      let applied = false;
      await fakeJson(page, VALIDATE, 'POST', {
        ok: true,
        warnings: [],
        diff: { storage: { 'buckets.db-archive.encryption.mode': { before: null, after: 'aes256-gcm-proxy' } } },
      });
      await page.route(path(STORAGE), async (route: Route) => {
        if (route.request().method() !== 'PUT') return route.fallback();
        applied = true;
        await route.fulfill({ json: { ok: true, warnings: [], requires_restart: false } });
      });
      await exampleOrigins(page, () => applied);
      await bucketKeyReady(page);
      await page.getByRole('button', { name: 'Review & apply' }).click();
      await page.getByRole('button', { name: /Apply and persist/i }).click();
      await page.getByRole('button', { name: /^Start now/ }).waitFor();
    },
    annotations: [{ target: { role: 'button', name: /^Start now/ }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'create-bucket-encryption',
    route: '/_/admin/storage/buckets',
    alt: 'The Create bucket dialog with the Encryption choice; the arrow points at Encrypt with a bucket key.',
    setup: async (page) => {
      await page.getByTestId('buckets-create').click();
      await page.getByTestId('bucket-name').fill('db-archive');
      await page.getByTestId('bucket-encryption-choice').waitFor();
      await page.mouse.move(1, 1);
    },
    annotations: [
      { target: { text: 'Encrypt with a bucket key', exact: true, within: { testId: 'bucket-encryption-choice' } }, kind: 'arrow', side: 'right' },
    ],
  },
  {
    id: 'bucket-encryption-chip',
    route: '/_/admin/storage/buckets',
    alt: 'The Buckets page; a box marks the chip Encrypted · bucket key db-archive-2026-10 of db-archive, and the downloads row shows Not encrypted.',
    setup: async (page) => {
      await exampleOrigins(page, () => true);
      await page.getByText('Encrypted · bucket key db-archive-2026-10').waitFor();
    },
    // The seed routes db-archive to local-disk; the example has it on hetzner-fsn1.
    mask: [{ text: /LOCAL-DISK/i }],
    annotations: [{ target: { text: 'Encrypted · bucket key db-archive-2026-10' }, kind: 'box' }],
  },
];
