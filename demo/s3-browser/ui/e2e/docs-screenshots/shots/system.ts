/**
 * System shots: the cards of the System page (docs: how-to/serve-tls,
 * how-to/go-to-production, how-to/run-multiple-instances,
 * how-to/view-live-logs, how-to/back-up-and-restore, how-to/upgrade).
 *
 * The setups change the form only; no shot applies a change, so the seeded
 * configuration stays the same for every shot.
 */
import type { Locator, Page } from '@playwright/test';
import type { MarkTarget, Shot, Target } from '../shot';

const reviewApply: Target = { role: 'button', name: 'Review & apply' };
const modal: Target = { role: 'dialog' };
const certPath = '/etc/ssl/certs/deltaglider.pem'; // placeholders of the TLS path fields
const keyPath = '/etc/ssl/private/deltaglider.key';
const syncField = 'Leave empty to disable';

/** Scroll the System page so that the element is in the middle of the view. */
async function center(t: Locator): Promise<void> {
  await t.evaluate((el) => el.scrollIntoView({ block: 'center' }));
}

async function blur(page: Page): Promise<void> {
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.mouse.move(1, 1);
}

async function fillTls(page: Page): Promise<void> {
  await page.getByRole('switch', { name: 'Enable TLS' }).click();
  await page.getByPlaceholder(certPath).fill('/etc/ssl/certs/proxy.pem');
  await page.getByPlaceholder(keyPath).fill('/etc/ssl/private/proxy-key.pem');
  await blur(page);
  await page.getByRole('button', { name: 'Review & apply' }).waitFor();
  await page.evaluate(() => window.scrollTo(0, 0));
}

async function fillCaches(page: Page): Promise<void> {
  await page.getByRole('spinbutton', { name: /Reference cache size/ }).fill('1024');
  await page.getByRole('spinbutton', { name: 'Metadata cache size (MB)' }).fill('200');
  await blur(page);
  await page.getByRole('button', { name: 'Review & apply' }).waitFor();
  await center(page.getByText('Caches', { exact: true }));
}

async function fillSync(page: Page): Promise<void> {
  await page.getByPlaceholder(syncField).fill('dgp-iam-sync');
  await blur(page);
  await page.getByRole('button', { name: 'Review & apply' }).waitFor();
  await center(page.getByText('Config DB sync', { exact: true }));
}

async function pickDebug(page: Page): Promise<void> {
  await page.locator('label.ant-radio-button-wrapper', { hasText: /^Debug$/ }).click();
  await blur(page);
  await page.getByRole('button', { name: 'Review & apply' }).waitFor();
  await center(page.getByText('Log level', { exact: true }));
}

/** Open the review dialog of the one dirty card. */
async function openReview(page: Page): Promise<void> {
  // dispatchEvent, not click: a click scrolls the page behind the dialog.
  await page.getByRole('button', { name: 'Review & apply' }).dispatchEvent('click');
  await page.getByTestId('apply-dialog-confirm').waitFor();
  // The inline dirty bar (z-index 1001) paints over the dialog (z-index
  // 1000); hide it so the shot shows the dialog alone.
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('span')) {
      if (el.textContent === 'Unsaved changes' && !el.closest('.ant-modal-wrap')) {
        (el.parentElement as HTMLElement).style.visibility = 'hidden';
      }
    }
    // The dialog focuses its close button; the focus ring is noise.
    (document.activeElement as HTMLElement | null)?.blur();
  });
}

async function openRestore(page: Page): Promise<void> {
  const chooser = page.waitForEvent('filechooser');
  await page.getByRole('button', { name: 'Restore backup' }).click();
  await (await chooser).setFiles({
    name: 'dgp-backup-20260612-090000.zip',
    mimeType: 'application/zip',
    // An empty zip: the dialog reads only the file name.
    buffer: Buffer.from([0x50, 0x4b, 0x05, 0x06, ...new Array(18).fill(0)]),
  });
  await page.getByRole('button', { name: 'Restore', exact: true }).waitFor();
}

const backupCard: MarkTarget = { union: [{ text: /^Download a full backup bundle/ }, { text: /^Restores can replace/ }] };

export const SYSTEM_SHOTS: Shot[] = [
  {
    id: 'tls-enable',
    route: '/_/admin/system',
    alt: 'The TLS card of the System page with TLS off; callout 2 marks the Enable TLS switch.',
    // Cropped to the card: the folder of screenshots has a size budget.
    clip: { union: [{ text: 'TLS', exact: true }, { text: /^Serve HTTPS directly/ }] },
    clipPadding: 48,
    annotations: [{ target: { role: 'switch', name: 'Enable TLS' }, kind: 'box', label: '2', side: 'right' }],
  },
  {
    id: 'tls-paths',
    route: '/_/admin/system',
    alt: 'TLS is on and both certificate paths are filled in; callout 3 marks the Certificate path and Private key path fields, and callout 4 marks Review & apply in the bar above the cards.',
    setup: fillTls,
    viewport: { width: 1040, height: 1000 },
    clip: { union: [reviewApply, { text: 'TLS', exact: true }, { placeholder: keyPath }] },
    clipPadding: 48,
    annotations: [
      { target: { union: [{ placeholder: certPath }, { placeholder: keyPath }] }, kind: 'box', label: '3', side: 'right' },
      { target: reviewApply, kind: 'box', label: '4', side: 'right' },
    ],
  },
  {
    id: 'tls-apply',
    route: '/_/admin/system',
    alt: 'The review dialog shows the TLS change and says that a restart is required; the arrow points at Apply and Persist.',
    setup: async (page) => {
      await fillTls(page);
      await openReview(page);
    },
    clip: modal,
    clipPadding: 110, // room for the arrow under the dialog
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'production-caches',
    route: '/_/admin/system',
    alt: 'The Caches card of the System page holds a reference cache of 1024 MB and a metadata cache of 200 MB; callout 1 marks the two cache fields and callout 2 marks Review & apply.',
    setup: fillCaches,
    clip: { union: [reviewApply, { text: 'Caches', exact: true }, { role: 'spinbutton', name: /Tokio blocking threads/ }] },
    clipPadding: 40,
    annotations: [
      {
        // Label to help text of both fields: the box must not cut through them.
        target: { union: [{ text: /^Reference cache size/ }, { text: /^LRU cache for delta-reconstruction/ }, { text: /^Object metadata cache for HEAD/ }] },
        kind: 'box',
        label: '1',
        side: 'right',
      },
      { target: reviewApply, kind: 'box', label: '2', side: 'right' },
    ],
  },
  {
    id: 'ha-sync-bucket',
    route: '/_/admin/system',
    alt: 'The Config DB sync card holds the sync bucket dgp-iam-sync and shows Pending restart; callout 1 marks the Sync bucket field and callout 2 marks Review & apply.',
    setup: fillSync,
    clip: { union: [reviewApply, { text: 'Config DB sync', exact: true }, { placeholder: syncField }] },
    // Room on the left of the card for callout 1.
    clipPadding: 72,
    annotations: [
      { target: { placeholder: syncField }, kind: 'box', label: '1', side: 'left' },
      { target: reviewApply, kind: 'box', label: '2', side: 'right' },
    ],
  },
  {
    id: 'ha-apply',
    route: '/_/admin/system',
    alt: 'The review dialog shows the new config_sync_bucket value and says that a restart is required; the arrow points at Apply and Persist.',
    setup: async (page) => {
      await fillSync(page);
      await openReview(page);
    },
    clip: modal,
    clipPadding: 110, // room for the arrow under the dialog
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'logs-level-debug',
    route: '/_/admin/system',
    alt: 'The Log level card of the System page has the Debug preset selected; callout 1 marks Debug and callout 2 marks Review & apply.',
    setup: pickDebug,
    clip: { union: [reviewApply, { text: 'Log level', exact: true }, { css: 'label.ant-radio-button-wrapper:has-text("Custom")' }] },
    clipPadding: 72,
    annotations: [
      { target: { css: 'label.ant-radio-button-wrapper:has-text("Debug")' }, kind: 'box', label: '1', side: 'bottom' },
      { target: reviewApply, kind: 'box', label: '2', side: 'right' },
    ],
  },
  {
    id: 'logs-apply',
    route: '/_/admin/system',
    alt: 'The review dialog shows that log_level changes to the debug filter; the arrow points at Apply and Persist.',
    setup: async (page) => {
      await pickDebug(page);
      await openReview(page);
    },
    clip: modal,
    clipPadding: 110, // room for the arrow under the dialog
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'backup-download',
    route: '/_/admin/system',
    alt: 'The backup card at the bottom of the System page; the box marks the Download backup button.',
    setup: (page) => center(page.getByRole('button', { name: 'Download backup' })),
    clip: backupCard,
    clipPadding: 40,
    // A box, not an arrow: the card is the last one, and an arrow crosses its text.
    annotations: [{ target: { role: 'button', name: 'Download backup' }, kind: 'box' }],
  },
  {
    id: 'backup-restore-open',
    route: '/_/admin/system',
    alt: 'The backup card at the bottom of the System page; the box marks the Restore backup button.',
    setup: (page) => center(page.getByRole('button', { name: 'Restore backup' })),
    clip: backupCard,
    clipPadding: 40,
    annotations: [{ target: { role: 'button', name: 'Restore backup' }, kind: 'box' }],
  },
  {
    id: 'backup-restore-modes',
    route: '/_/admin/system',
    alt: 'The Restore backup dialog lists what to restore; callout 1 marks the four restore modes, callout 2 marks Replace and Merge for the users and groups that exist now, and callout 3 marks the Restore button.',
    setup: openRestore,
    clip: modal,
    clipPadding: 48,
    annotations: [
      {
        target: { css: '.ant-modal .ant-radio-group', nth: 0 },
        kind: 'box',
        label: '1',
        side: 'right',
      },
      { target: { role: 'radiogroup', name: 'Users, groups and OIDC providers that exist now' }, kind: 'box', label: '2', side: 'right' },
      { target: { role: 'button', name: 'Restore', exact: true }, kind: 'box', label: '3', side: 'right' },
    ],
  },
];
