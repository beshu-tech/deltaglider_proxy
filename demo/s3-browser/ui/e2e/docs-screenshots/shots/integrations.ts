/**
 * Integrations shots: Event delivery and the Event log (docs:
 * how-to/send-event-notifications).
 *
 * Only the last shot changes the proxy: it turns delivery on towards a
 * receiver that refuses the connection, so the Event log has a failed row to
 * requeue. Keep it last, here and in the capture order.
 */
import { PutObjectCommand, S3Client } from '@aws-sdk/client-s3';
import type { Page } from '@playwright/test';
import type { Shot } from '../shot';
import { BASE } from '../seed';

const ROUTE = '/_/admin/integrations/event-delivery';

async function blur(page: Page): Promise<void> {
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.mouse.move(1, 1);
}

/** Delivery on, one endpoint with a bearer header, local receivers allowed. */
async function fillWebhook(page: Page): Promise<void> {
  await page.getByRole('switch', { name: 'Enable delivery' }).click();
  await page.getByRole('button', { name: 'Add endpoint' }).click();
  await page.getByRole('textbox', { name: 'Endpoint URL' }).fill('https://events.acme.example/deltaglider');
  await page.getByRole('button', { name: 'Add header' }).click();
  await page.getByRole('textbox', { name: 'Header name' }).fill('Authorization');
  await page.getByLabel('Header value').fill('Bearer events-token');
  await blur(page);
  await page.getByRole('button', { name: 'Review & apply' }).waitFor();
}

/** Delivery on in Slack format, in the given connection mode. */
async function slack(page: Page, mode: 'webhook' | 'bot'): Promise<void> {
  await page.getByRole('switch', { name: 'Enable delivery' }).click();
  await page.locator('label.ant-radio-button-wrapper', { hasText: /^Slack$/ }).click();
  if (mode === 'webhook') {
    await page.getByRole('button', { name: 'Add webhook URL' }).click();
    await page.getByRole('textbox', { name: 'Incoming Webhook URL 1' }).fill('https://hooks.slack.com/services/T000/B000/XXXX');
  } else {
    await page.locator('label.ant-radio-button-wrapper', { hasText: /^Bot token/ }).click();
    await page.getByPlaceholder('xoxb-…').fill('xoxb-0000-0000-docs');
    await page.getByPlaceholder('#deploys or C0123ABC').fill('#ops');
  }
  await blur(page);
}

/** The Requeue button of a row; its icon adds "sync" to the accessible name. */
const requeue = (page: Page) => page.getByRole('button', { name: /^(sync )?Requeue$/ });

export const INTEGRATIONS_SHOTS: Shot[] = [
  {
    id: 'events-enable',
    route: ROUTE,
    alt: 'The Event delivery page with delivery off; callout 2 marks the Enable delivery switch, and Payload format is on Raw webhook.',
    // Cropped to the delivery card: the folder of screenshots has a size budget.
    clip: { union: [{ css: '.ant-alert' }, { text: /^Raw posts the deltaglider/ }] },
    clipPadding: 32,
    annotations: [{ target: { role: 'switch', name: 'Enable delivery' }, kind: 'box', label: '2', side: 'right' }],
  },
  {
    id: 'events-endpoint',
    route: ROUTE,
    alt: 'The raw webhook destination holds one endpoint and an Authorization header; callout 3 marks the endpoint URL, callout 4 marks the Allow local receivers switch, and callout 5 marks the header.',
    setup: async (page) => {
      await fillWebhook(page);
      await page.getByRole('textbox', { name: 'Endpoint URL' }).evaluate((el) => el.scrollIntoView({ block: 'center' }));
    },
    clip: { union: [{ text: 'Endpoints', exact: true }, { role: 'textbox', name: 'Endpoint URL' }, { role: 'button', name: /Add header/ }, { label: 'Header value' }] },
    clipPadding: 72,
    annotations: [
      { target: { role: 'textbox', name: 'Endpoint URL' }, kind: 'box', label: '3', side: 'right' },
      { target: { union: [{ role: 'textbox', name: 'Header name' }, { label: 'Header value' }] }, kind: 'box', label: '5', side: 'right' },
      { target: { role: 'switch', name: 'Allow local receivers' }, kind: 'box', label: '4', side: 'right' },
    ],
  },
  {
    id: 'events-apply',
    route: ROUTE,
    alt: 'The review dialog shows the event_delivery change with the endpoint and the masked header; the arrow points at Apply and Persist.',
    setup: async (page) => {
      await fillWebhook(page);
      // dispatchEvent, not click: a click scrolls the page behind the dialog.
      await page.getByRole('button', { name: 'Review & apply' }).dispatchEvent('click');
      await page.getByTestId('apply-dialog-confirm').waitFor();
      await blur(page);
    },
    clip: { role: 'dialog' },
    clipPadding: 110, // room for the arrow under the dialog
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'events-slack-webhook',
    route: ROUTE,
    alt: 'Event delivery in Slack format with an Incoming Webhook; callout 1 marks Slack in Payload format, callout 2 marks How to connect, and callout 3 marks the Incoming Webhook URL.',
    setup: async (page) => {
      await slack(page, 'webhook');
      await page.getByText('Payload format', { exact: true }).evaluate((el) => el.scrollIntoView({ block: 'start' }));
    },
    viewport: { width: 1280, height: 1000 },
    clip: { union: [{ text: 'Payload format', exact: true }, { css: 'button[title="Remove webhook URL"]' }, { role: 'button', name: /Add webhook URL/ }] },
    clipPadding: 56,
    annotations: [
      { target: { css: 'label.ant-radio-button-wrapper:has-text("Slack")' }, kind: 'box', label: '1', side: 'right' },
      { target: { text: 'How to connect', exact: true }, kind: 'callout', label: '2', side: 'right' },
      {
        target: { union: [{ role: 'textbox', name: 'Incoming Webhook URL 1' }, { css: 'button[title="Remove webhook URL"]' }] },
        kind: 'box',
        label: '3',
        side: 'right',
      },
    ],
  },
  {
    id: 'events-slack-bot',
    route: ROUTE,
    alt: 'Event delivery in Slack format with a bot token; callout 1 marks Bot token in How to connect, callout 2 marks the Bot token field, and callout 3 marks the Channel field.',
    setup: async (page) => {
      await slack(page, 'bot');
      await page.getByPlaceholder('#deploys or C0123ABC').evaluate((el) => el.scrollIntoView({ block: 'center' }));
    },
    clip: { union: [{ text: 'How to connect', exact: true }, { text: /^Channel id \(like/ }, { css: 'label.ant-radio-button-wrapper:has-text("Bot token")' }] },
    clipPadding: 56,
    annotations: [
      { target: { css: 'label.ant-radio-button-wrapper:has-text("Bot token")' }, kind: 'box', label: '1', side: 'top' },
      { target: { css: 'input[value^="xoxb-"]' }, kind: 'box', label: '2', side: 'right' },
      { target: { placeholder: '#deploys or C0123ABC' }, kind: 'box', label: '3', side: 'right' },
    ],
  },
  {
    id: 'events-slack-filter',
    route: ROUTE,
    alt: 'The What gets posted section of the Slack connector is open; the box marks the event kinds and the include and exclude prefix filters.',
    setup: async (page) => {
      await slack(page, 'webhook');
      const summary = page.getByText('What gets posted (event kinds + prefix filters)', { exact: true });
      await summary.click();
      await page.getByRole('button', { name: 'Add prefix glob' }).first().click();
      await page.getByRole('textbox', { name: 'Include prefixes 1' }).fill('firmware/**');
      await page.getByRole('button', { name: 'Add prefix glob' }).nth(1).click();
      await page.getByRole('textbox', { name: 'Exclude prefixes 1' }).fill('**/*.tmp');
      await blur(page);
      await page.getByText('Event kinds', { exact: true }).evaluate((el) => el.scrollIntoView({ block: 'start' }));
    },
    annotations: [
      {
        target: { union: [{ text: 'Event kinds', exact: true }, { role: 'textbox', name: 'Exclude prefixes 1' }] },
        kind: 'box',
      },
    ],
  },
  {
    id: 'events-log-requeue',
    route: '/_/admin/integrations/event-outbox',
    // Wide enough for the Action column of the table; cropped to the table.
    viewport: { width: 1440, height: 800 },
    clip: { union: [{ text: 'Object change events', exact: true }, { text: /^1-\d+ of \d+$/ }, { role: 'button', name: /^(sync )?Requeue$/, nth: 0 }] },
    clipPadding: 32,
    alt: 'The Event log lists an event whose delivery failed; the arrow points at the Requeue button of that row.',
    setup: async (page) => {
      const headers = { Origin: BASE };
      const put = await page.request.put('/_/api/admin/config/section/advanced', {
        headers,
        data: {
          event_delivery: {
            enabled: true,
            // Port 9 (discard) is closed: every attempt fails at once.
            webhook_urls: ['http://127.0.0.1:9/deltaglider'],
            allow_local: true,
            max_attempts: 1,
            tick_interval: '1s',
          },
        },
      });
      if (!put.ok()) throw new Error(`event_delivery: HTTP ${put.status()} ${await put.text()}`);
      const s3 = new S3Client({
        endpoint: BASE,
        region: 'us-east-1',
        forcePathStyle: true,
        credentials: {
          accessKeyId: process.env.E2E_ACCESS_KEY ?? 'qa-admin-key',
          secretAccessKey: process.env.E2E_SECRET_KEY ?? 'qa-admin-secret-0123456789',
        },
      });
      const failed = async () => {
        const r = await page.request.get('/_/api/admin/event-outbox?limit=50');
        return ((await r.json()) as { counts?: { failed?: number } }).counts?.failed ?? 0;
      };
      // One probe for both themes: the second capture must see the same rows.
      if ((await failed()) === 0) {
        await s3.send(new PutObjectCommand({ Bucket: 'releases', Key: 'reports/probe.txt', Body: 'probe\n' }));
      }
      s3.destroy();
      const end = Date.now() + 60_000;
      for (;;) {
        // Two rows: the upload, and the copy that event-driven replication
        // makes into releases-dr (its event arrives a moment later).
        if ((await failed()) >= 2) break;
        if (Date.now() > end) throw new Error('events-log-requeue: no failed event after 60 s');
        await new Promise((res) => setTimeout(res, 500));
      }
      await page.reload();
      // Show the failed rows only, so the first Requeue button is enabled.
      await page.getByRole('combobox', { name: 'Status filter' }).click();
      await page.locator('.ant-select-item-option', { hasText: /^Failed/ }).click();
      await requeue(page).first().waitFor();
    },
    annotations: [{ target: { role: 'button', name: /^(sync )?Requeue$/, nth: 0 }, kind: 'arrow', side: 'left' }],
  },
];
