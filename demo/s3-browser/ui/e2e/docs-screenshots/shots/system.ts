/**
 * System shots: the System page cards (docs: how-to/serve-tls,
 * how-to/back-up-and-restore, reference/configuration).
 */
import type { Shot } from '../shot';

export const SYSTEM_SHOTS: Shot[] = [
  {
    id: 'system-listener-tls',
    route: '/_/admin/system',
    alt: 'The System page shows the address that the proxy listens on and the TLS card; the box marks the Enable TLS switch.',
    annotations: [{ target: { role: 'switch', name: 'Enable TLS' }, kind: 'box' }],
  },
  {
    id: 'system-backup',
    route: '/_/admin/system',
    alt: 'The Backup card of the System page downloads a full backup and restores one from a file; the box marks the Download backup and Restore backup buttons.',
    setup: async (page) => {
      await page.getByRole('button', { name: 'Download backup' }).evaluate((el) => el.scrollIntoView({ block: 'center' }));
    },
    // The System page scrolls as one document: crop to the card.
    clip: { union: [{ text: /^Download a full backup bundle/ }, { text: /^Restores can replace/ }] },
    clipPadding: 40,
    annotations: [
      { target: { union: [{ role: 'button', name: 'Download backup' }, { role: 'button', name: 'Restore backup' }] }, kind: 'box' },
    ],
  },
];
