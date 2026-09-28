/**
 * Storage shots: Backends and Buckets (docs: how-to/route-a-bucket-to-a-backend,
 * explanation/multi-backend-architecture, how-to/set-bucket-compression-and-quotas).
 */
import type { Page } from '@playwright/test';
import type { Shot, Target } from '../shot';
import { nav } from './common';

/** The collapsed status row of a bucket on the Buckets page. */
const bucketRow = (name: string): Target => ({ role: 'button', name: new RegExp(`^${name} — click to`) });

async function openBucket(page: Page, name: string): Promise<void> {
  await page.getByRole('button', { name: new RegExp(`^${name} — click to edit`) }).click();
  await page.getByRole('button', { name: new RegExp(`^${name} — click to collapse`) }).waitFor();
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
    viewport: { width: 1280, height: 900 },
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
    alt: 'The db-archive bucket is open with its Advanced settings; callout 1 marks the bucket row, callout 2 marks Advanced, and callout 3 marks Real name on backend, set to acme-db-archive-prod.',
    setup: async (page) => {
      await openBucket(page, 'db-archive');
      await page.getByText(/^Advanced/).click();
      await page.getByRole('textbox', { name: 'Real name on backend' }).fill('acme-db-archive-prod');
    },
    annotations: [
      { target: bucketRow('db-archive'), kind: 'callout', label: '1' },
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
  {
    id: 'bucket-quota',
    route: '/_/admin/storage/buckets',
    alt: 'The releases bucket row is open with its Advanced settings; the box marks the quota of 50 GB.',
    setup: async (page) => {
      await openBucket(page, 'releases');
      await page.getByRole('spinbutton', { name: 'Quota' }).scrollIntoViewIfNeeded();
    },
    annotations: [{ target: { role: 'spinbutton', name: 'Quota' }, kind: 'box' }],
  },
  {
    id: 'bucket-public-prefix',
    route: '/_/admin/storage/buckets',
    alt: 'The downloads bucket row makes only the public/ prefix readable without credentials; the box marks the Specific prefixes public setting.',
    setup: (page) => openBucket(page, 'downloads'),
    annotations: [
      {
        target: { union: [{ text: 'Specific prefixes public', exact: true }, { css: 'input[value="public/"]' }] },
        kind: 'box',
      },
    ],
  },
];
