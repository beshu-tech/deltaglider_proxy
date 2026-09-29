/**
 * Storage shots: Backends and Buckets (docs: how-to/route-a-bucket-to-a-backend,
 * explanation/multi-backend-architecture, how-to/set-bucket-compression-and-quotas).
 */
import type { APIRequestContext, Page } from '@playwright/test';
import type { Shot, Target } from '../shot';
import { nav } from './common';
import { BASE } from '../seed';

/** The collapsed status row of a bucket on the Buckets page. */
const bucketRow = (name: string): Target => ({ role: 'button', name: new RegExp(`^${name} — click to`) });

async function openBucket(page: Page, name: string): Promise<void> {
  await page.getByRole('button', { name: new RegExp(`^${name} — click to edit`) }).click();
  await page.getByRole('button', { name: new RegExp(`^${name} — click to collapse`) }).waitFor();
}

/** Open the Advanced disclosure of the open bucket card (it stays open when a setting in it is in use). */
async function openAdvanced(page: Page): Promise<void> {
  const cutoff = page.getByRole('spinbutton', { name: 'Quota' });
  if (!(await cutoff.isVisible())) await page.getByText(/^Advanced/).click();
  await cutoff.waitFor();
}

/** The Backend select inside the open bucket card. */
const backendSelect = (page: Page) => page.getByRole('combobox', { name: 'Backend' });
const option = (page: Page, title: string) =>
  page.locator(`.ant-select-dropdown .ant-select-item-option[title="${title}"]`);

async function routeDownloadsToLocalDisk(page: Page): Promise<void> {
  await openBucket(page, 'downloads');
  await backendSelect(page).click();
  await option(page, 'local-disk').click();
  await page.getByRole('button', { name: 'Review & apply' }).waitFor();
  // Reaching the option may scroll the page by a different amount on each
  // run; put it back at the top, where the downloads row is.
  await page.evaluate(() => window.scrollTo(0, 0));
}

async function openNewS3Backend(page: Page): Promise<void> {
  await page.getByTestId('backends-add').click();
  await page.getByTestId('backend-name').fill('hetzner-fsn1');
  await page.getByText('S3', { exact: true }).click();
  await page.getByPlaceholder('https://fsn1.your-objectstorage.com').fill('https://fsn1.your-objectstorage.com');
  await page.getByPlaceholder('us-east-1').fill('fsn1');
  await page.getByPlaceholder('AKIAIOSFODNN7EXAMPLE').fill('HETZNER-ACCESS-KEY-ID');
  await page.getByPlaceholder('wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLE').fill('hetzner-secret-access-key');
  await page.getByPlaceholder('us-east-1').evaluate((el) => el.scrollIntoView({ block: 'center' }));
  // No focus ring, no hover chip: the form as it looks before the click.
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.mouse.move(1, 1);
}

export const STORAGE_SHOTS: Shot[] = [
  {
    id: 'route-bucket-add-backend',
    route: '/_/admin/storage/backends',
    alt: 'The Backends page lists the configured backends; callout 1 marks Backends in the sidebar and callout 2 marks the Add Backend button.',
    annotations: [
      { target: nav('Backends'), kind: 'callout', label: '1', side: 'right' },
      { target: { testId: 'backends-add' }, kind: 'box', label: '2' },
    ],
  },
  {
    id: 'route-bucket-backend-form',
    route: '/_/admin/storage/backends',
    alt: 'The New Backend form holds the S3 settings of hetzner-fsn1; callout 3 marks the S3 fields and callout 4 marks the Create Backend button.',
    setup: openNewS3Backend,
    // The form is taller than what is left of the screen under the list:
    // scroll it to the middle, then crop to it.
    viewport: { width: 1040, height: 900 },
    clip: { union: [{ text: 'New Backend', exact: true }, { testId: 'backend-create' }] },
    clipPadding: 56,
    annotations: [
      {
        // From the Endpoint field down to the Secret Access Key field.
        target: {
          union: [
            { text: 'Endpoint', exact: true },
            { placeholder: 'us-east-1' }, // full width: sets the right edge
            { placeholder: 'wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLE' },
          ],
        },
        kind: 'box',
        label: '3',
      },
      { target: { testId: 'backend-create' }, kind: 'box', label: '4' },
    ],
  },
  {
    id: 'route-bucket-buckets-row',
    route: '/_/admin/storage/buckets',
    alt: 'The Buckets page shows one row per bucket with its backend; callout 1 marks Buckets in the sidebar and callout 2 marks the downloads row.',
    annotations: [
      { target: nav('Buckets'), kind: 'callout', label: '1', side: 'right' },
      { target: bucketRow('downloads'), kind: 'box', label: '2' },
    ],
  },
  {
    id: 'route-bucket-backend-select',
    route: '/_/admin/storage/buckets',
    alt: 'The downloads bucket is open and its Backend list is expanded; the arrow points at the local-disk option.',
    setup: async (page) => {
      await openBucket(page, 'downloads');
      await backendSelect(page).click();
      await option(page, 'local-disk').waitFor();
    },
    annotations: [
      { target: { css: '.ant-select-dropdown .ant-select-item-option[title="local-disk"]' }, kind: 'arrow', side: 'right' },
    ],
  },
  {
    id: 'route-bucket-review-apply',
    route: '/_/admin/storage/buckets',
    alt: 'The downloads bucket now routes to local-disk, and a bar at the bottom of the page holds the unsaved change; the arrow points at Review & apply.',
    setup: routeDownloadsToLocalDisk,
    annotations: [{ target: { role: 'button', name: 'Review & apply' }, kind: 'arrow', side: 'top' }],
  },
  {
    id: 'route-bucket-apply-dialog',
    route: '/_/admin/storage/buckets',
    alt: 'The review dialog shows that the backend of the downloads bucket changes to local-disk; the arrow points at Apply and Persist.',
    setup: async (page) => {
      await routeDownloadsToLocalDisk(page);
      // dispatchEvent, not click: a click scrolls the page to the sticky
      // bar's place in the flow, and the page behind the dialog moves.
      await page.getByRole('button', { name: 'Review & apply' }).dispatchEvent('click');
      await page.getByTestId('apply-dialog-confirm').waitFor();
    },
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'route-bucket-alias',
    route: '/_/admin/storage/buckets',
    alt: 'The releases bucket is open with its Advanced settings; callout 1 marks the bucket row, callout 2 marks Advanced, and callout 3 marks Real name on backend, set to acme-prod-releases-fsn1.',
    setup: async (page) => {
      await openBucket(page, 'releases');
      // releases has a quota, so its Advanced part is open already.
      await openAdvanced(page);
      await page.getByRole('textbox', { name: 'Real name on backend' }).fill('acme-prod-releases-fsn1');
    },
    annotations: [
      { target: bucketRow('releases'), kind: 'callout', label: '1' },
      { target: { text: /^Advanced/ }, kind: 'callout', label: '2', side: 'right' },
      { target: { role: 'textbox', name: 'Real name on backend' }, kind: 'box', label: '3', side: 'right' },
    ],
  },
  {
    id: 'backends-three',
    route: '/_/admin/storage/backends',
    alt: 'The Backends page lists hetzner-fsn1, local-disk and aws-dr; the arrow points at hetzner-fsn1, the default backend.',
    annotations: [{ target: { text: 'hetzner-fsn1', exact: true }, kind: 'arrow', side: 'left' }],
  },
  {
    id: 'buckets-routing',
    route: '/_/admin/storage/buckets',
    alt: 'The Buckets page shows the backend of each bucket; the db-archive row is open, and the box marks its Backend list, set to local-disk.',
    setup: (page) => openBucket(page, 'db-archive'),
    annotations: [{ target: { role: 'combobox', name: 'Backend' }, kind: 'box' }],
  },
];

async function compressionOffOnDownloads(page: Page): Promise<void> {
  await openBucket(page, 'downloads');
  await openAdvanced(page);
  await page.getByRole('combobox', { name: 'Compression' }).click();
  await option(page, 'Off').click();
  await page.getByRole('button', { name: 'Review & apply' }).waitFor();
  await page.evaluate(() => window.scrollTo(0, 0));
}

/** Set the public prefixes of downloads through the admin API (a storage merge-patch). */
async function setDownloadsPublicPrefixes(api: APIRequestContext, prefixes: string[]): Promise<void> {
  const r = await api.put('/_/api/admin/config/section/storage', {
    data: { buckets: { downloads: { public_prefixes: prefixes } } },
    headers: { Origin: BASE },
  });
  if (!r.ok()) throw new Error(`public prefixes of downloads: HTTP ${r.status()} ${await r.text()}`);
}

async function reviewDialog(page: Page): Promise<void> {
  // dispatchEvent, not click: see route-bucket-apply-dialog.
  await page.getByRole('button', { name: 'Review & apply' }).dispatchEvent('click');
  await page.getByTestId('apply-dialog-confirm').waitFor();
}

/** A new settings row for a bucket that the proxy does not see yet. */
async function openDraft(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'More ways to add a bucket' }).click();
  await page.getByText('Add settings for a bucket that does not exist yet').click();
  await page.getByPlaceholder('Bucket name').waitFor();
}

const card = (name: string): Target => ({ css: `[data-backend-card="${name}"]` });

/**
 * Batch A shots (docs: how-to/set-bucket-compression-and-quotas,
 * publish-a-public-folder, migrate-existing-data-into-the-proxy,
 * backend-capability-validation, diagnose-backend-connectivity).
 */
const BUCKET_POLICY_SHOTS: Shot[] = [
  {
    id: 'bucket-compression-off',
    route: '/_/admin/storage/buckets',
    alt: 'The downloads bucket is open with its Advanced settings; callout 3 marks Advanced and callout 4 marks the Compression list, set to Off.',
    setup: compressionOffOnDownloads,
    annotations: [
      { target: { text: /^Advanced/ }, kind: 'callout', label: '3', side: 'right' },
      { target: { role: 'combobox', name: 'Compression' }, kind: 'box', label: '4', side: 'right' },
    ],
  },
  {
    id: 'bucket-compression-apply-dialog',
    route: '/_/admin/storage/buckets',
    alt: 'The review dialog shows that compression of the downloads bucket changes to false; the arrow points at Apply and Persist.',
    setup: async (page) => {
      await compressionOffOnDownloads(page);
      await reviewDialog(page);
    },
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'bucket-delta-cutoff',
    route: '/_/admin/storage/buckets',
    alt: 'The releases bucket is open with its Advanced settings; callout 1 marks the releases row and callout 2 marks Delta size cutoff, set to 0.5.',
    setup: async (page) => {
      await openBucket(page, 'releases');
      await openAdvanced(page);
      await page.getByRole('spinbutton', { name: 'Delta size cutoff' }).fill('0.5');
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
      await page.evaluate(() => window.scrollTo(0, 0));
    },
    annotations: [
      { target: bucketRow('releases'), kind: 'callout', label: '1' },
      { target: { role: 'spinbutton', name: 'Delta size cutoff' }, kind: 'box', label: '2', side: 'right' },
    ],
  },
  {
    id: 'bucket-quota',
    route: '/_/admin/storage/buckets',
    alt: 'The db-archive bucket is open with its Advanced settings; callout 1 marks the db-archive row, callout 2 marks Advanced, and callout 3 marks Quota, set to 500 GiB.',
    setup: async (page) => {
      await openBucket(page, 'db-archive');
      await openAdvanced(page);
      await page.getByRole('spinbutton', { name: 'Quota' }).fill('500');
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
      await page.evaluate(() => window.scrollTo(0, 0));
    },
    annotations: [
      { target: bucketRow('db-archive'), kind: 'callout', label: '1' },
      { target: { text: /^Advanced/ }, kind: 'callout', label: '2', side: 'right' },
      // Box the whole InputNumber, not the inner <input>: a callout to the
      // right of the input covered the GiB suffix.
      { target: { css: '.ant-input-number:has(input[placeholder="Unlimited"])' }, kind: 'box', label: '3', side: 'right' },
    ],
  },
  {
    // The seed publishes downloads/public/ (the rule tester shots need it), so
    // there would be nothing to apply: take the prefix away for this shot and
    // give it back after.
    id: 'bucket-public-prefix-apply',
    route: '/_/admin/storage/buckets',
    alt: 'The review dialog shows that the downloads bucket gets the public prefix public/; the arrow points at Apply and Persist.',
    setup: async (page) => {
      await setDownloadsPublicPrefixes(page.request, []);
      await page.reload();
      await openBucket(page, 'downloads');
      await page.getByText('Specific prefixes public', { exact: true }).click();
      await page.getByRole('textbox', { name: 'Public prefix' }).fill('public/');
      await reviewDialog(page);
    },
    teardown: (api) => setDownloadsPublicPrefixes(api, ['public/']),
    clip: { role: 'dialog' },
    clipPadding: 16,
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'bucket-public-prefix',
    route: '/_/admin/storage/buckets',
    alt: 'The downloads bucket is open; callout 3 marks the Specific prefixes public option and callout 4 marks the prefix field, which holds public/.',
    setup: (page) => openBucket(page, 'downloads'),
    annotations: [
      { target: { text: 'Specific prefixes public', exact: true }, kind: 'callout', label: '3', side: 'right' },
      { target: { role: 'textbox', name: 'Public prefix' }, kind: 'box', label: '4', side: 'right' },
    ],
  },
  {
    id: 'adopt-bucket-draft',
    route: '/_/admin/storage/buckets',
    alt: 'The Buckets page with the menu next to Create bucket open; callout 1 marks Buckets in the sidebar, callout 2 marks the menu button, and callout 3 marks Add settings for a bucket that does not exist yet.',
    setup: async (page) => {
      await page.getByRole('button', { name: 'More ways to add a bucket' }).click();
      await page.getByText('Add settings for a bucket that does not exist yet').waitFor();
    },
    annotations: [
      { target: nav('Buckets'), kind: 'callout', label: '1', side: 'right' },
      { target: { role: 'button', name: 'More ways to add a bucket' }, kind: 'callout', label: '2', side: 'right' },
      { target: { text: 'Add settings for a bucket that does not exist yet' }, kind: 'callout', label: '3', side: 'right' },
    ],
  },
  {
    id: 'adopt-bucket-alias',
    route: '/_/admin/storage/buckets',
    alt: 'A new settings row is open; callout 4 marks the Bucket name field, callout 5 marks the Backend list, set to aws-dr, callout 6 marks Advanced, and callout 7 marks Real name on backend, set to acme-firmware.',
    // The new row sits below the four buckets: a taller screen shows all of it.
    viewport: { width: 1040, height: 1000 },
    setup: async (page) => {
      await openDraft(page);
      await backendSelect(page).click();
      await option(page, 'aws-dr').click();
      await page.getByText(/^Advanced/).click();
      await page.getByRole('textbox', { name: 'Real name on backend' }).fill('acme-firmware');
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
      await backendSelect(page).evaluate((el) => el.scrollIntoView({ block: 'center' }));
      await page.mouse.move(1, 1);
    },
    annotations: [
      { target: { placeholder: 'Bucket name' }, kind: 'callout', label: '4', side: 'right' },
      { target: { role: 'combobox', name: 'Backend' }, kind: 'box', label: '5', side: 'right' },
      { target: { text: /^Advanced/ }, kind: 'callout', label: '6', side: 'right' },
      { target: { role: 'textbox', name: 'Real name on backend' }, kind: 'box', label: '7', side: 'right' },
    ],
  },
  {
    id: 'backend-cas-reroute',
    route: '/_/admin/storage/buckets',
    alt: 'The db-archive bucket is open and its Backend list is expanded; the arrow points at hetzner-fsn1, a backend with conditional writes.',
    setup: async (page) => {
      await openBucket(page, 'db-archive');
      await backendSelect(page).click();
      await option(page, 'hetzner-fsn1').waitFor();
    },
    annotations: [
      { target: { css: '.ant-select-dropdown .ant-select-item-option[title="hetzner-fsn1"]' }, kind: 'arrow', side: 'right' },
    ],
  },
  {
    id: 'backend-health-test-connection',
    route: '/_/admin/storage/backends',
    alt: 'The hetzner-fsn1 backend card after a probe; a numbered mark sits above its Connected badge, and boxes mark its Test connection button and the probe result.',
    setup: async (page) => {
      await page.getByRole('button', { name: /Test connection$/ }).and(page.locator('[data-backend-card="hetzner-fsn1"] button')).click();
      await page.locator('[data-backend-card="hetzner-fsn1"] .ant-alert').first().waitFor();
      await page.mouse.move(1, 1);
    },
    annotations: [
      // The badge comes first in the card; the probe result repeats the word.
      // A callout above it: a box frame would cross the endpoint line below.
      { target: { text: 'Connected', exact: true, within: card('hetzner-fsn1'), nth: 0 }, kind: 'callout', label: '1', side: 'top' },
      { target: { css: '[data-backend-card="hetzner-fsn1"] .ant-alert-success' }, kind: 'box' },
      // A box, not an arrow: every side of the button has text within an arrow's length.
      { target: { role: 'button', name: /Test connection$/, within: card('hetzner-fsn1') }, kind: 'box' },
    ],
  },
];

STORAGE_SHOTS.push(...BUCKET_POLICY_SHOTS);
