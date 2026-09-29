/**
 * Object browser shots (docs: README, tutorials/first-delta-savings,
 * explanation/delta-compression, how-to/browse-share-and-bulk-edit-objects).
 */
import type { Locator } from '@playwright/test';
import type { Shot } from '../shot';

async function stableText(loc: Locator): Promise<void> {
  let last = '';
  for (let i = 0; i < 60; i++) {
    const now = (await loc.textContent()) ?? '';
    if (now && now === last) return;
    last = now;
    await loc.page().waitForTimeout(150);
  }
}

const FW = '/_/browse/releases/firmware/widget-3000/';

export const BROWSER_SHOTS: Shot[] = [
  {
    id: 'browser-releases',
    viewport: { width: 1280, height: 820 }, // the layout drops this control below 1280 px
    route: FW,
    alt: 'The object browser lists four firmware tarballs in the releases bucket, each stored as a delta; the box marks how much smaller the folder is than its original size.',
    // The chip counts up to its value; wait until the text stops changing.
    setup: (page) => stableText(page.getByRole('status', { name: /deltas? of \d+ objects/ })),
    annotations: [{ target: { role: 'status', name: /deltas? of \d+ objects/ }, kind: 'box' }],
  },
  {
    id: 'inspector-delta-savings',
    route: `${FW}?object=firmware/widget-3000/fw-1.4.1.tar`,
    alt: 'The object inspector shows that fw-1.4.1.tar is stored as a delta of about 48 KB for a 3 MB file; the box marks the savings.',
    annotations: [{ target: { testId: 'inspector-savings' }, kind: 'box' }],
    mask: [{ text: /^\d{1,2}\/\d{1,2}\/\d{4}, / }],
  },
  {
    id: 'command-palette',
    // Not the dashboard: its live numbers change on every run.
    route: '/_/admin/access/admission',
    alt: 'The command palette finds the Jobs page after three typed letters; the box marks the search field.',
    setup: async (page) => {
      await page.keyboard.press('Control+K');
      await page.getByRole('combobox', { name: 'Search pages and actions' }).or(page.getByPlaceholder('Type to filter pages or actions...')).fill('job');
    },
    annotations: [{ target: { placeholder: 'Type to filter pages or actions...' }, kind: 'box' }],
  },
  {
    id: 'shortcuts-help',
    route: '/_/admin/jobs',
    viewport: { width: 1040, height: 1400 },
    clip: { role: 'dialog' },
    clipPadding: 24,
    alt: 'The shortcuts dialog lists the keyboard shortcuts of the admin pages.',
    setup: async (page) => {
      await page.locator('body').press('?');
      await page.getByRole('dialog').waitFor();
    },
  },
];
