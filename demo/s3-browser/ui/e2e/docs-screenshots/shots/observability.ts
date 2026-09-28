/**
 * Observability shots: the rule tester, the audit log, compression health,
 * the live logs (docs: how-to/trace-requests,
 * how-to/gate-requests-with-admission-rules, how-to/view-live-logs).
 */
import type { Shot } from '../shot';

export const OBSERVABILITY_SHOTS: Shot[] = [
  {
    id: 'rule-tester',
    route: '/_/admin/diagnostics/trace',
    alt: 'The rule tester shows that an anonymous GET of downloads/public/installer.sh is allowed; the box marks the decision and the rule that made it.',
    setup: async (page) => {
      await page.getByPlaceholder('/my-bucket/some/key').fill('/downloads/public/installer.sh');
      await page.getByRole('button', { name: 'Test request' }).click();
      await page.getByText('Reason path').waitFor();
      await page.getByText('Decision', { exact: true }).evaluate((el) => el.scrollIntoView({ block: 'start' }));
    },
    // The result is below the form: crop to the result card.
    clip: { union: [{ text: 'Decision', exact: true }, { role: 'button', name: 'Copy as JSON' }] },
    clipPadding: 48,
    annotations: [{ target: { css: 'text="by rule" >> xpath=..' }, kind: 'box' }],
  },
  {
    id: 'audit-log',
    route: '/_/admin/diagnostics/audit',
    alt: 'The audit log lists recent admin actions with the user, the source address and the target of each one; the box marks the filter field.',
    annotations: [{ target: { role: 'textbox', name: 'Filter audit entries' }, kind: 'box' }],
  },
  {
    id: 'logs-follow',
    route: '/_/admin/diagnostics/logs',
    // The startup line carries the crate version, which no shot may show:
    // filter the view to one replication line of the seed (a short list
    // leaves room for the callouts).
    setup: async (page) => {
      await page.getByRole('textbox', { name: 'Search message and fields' }).fill('Replication run finished');
      await page.waitForTimeout(1500);
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    },
    alt: 'The System logs page shows a filtered log line; callout 2 marks the level, target and search filters, and callout 3 marks the Follow switch.',
    // Cropped to the filters and the first lines: the folder of screenshots has a size budget.
    clip: { union: [{ role: 'combobox', name: 'Minimum level' }, { text: /lines$/ }] },
    clipPadding: 80,
    annotations: [
      {
        target: { union: [{ role: 'combobox', name: 'Minimum level' }, { role: 'textbox', name: 'Search message and fields' }] },
        kind: 'box',
        label: '2',
        side: 'right',
      },
      { target: { union: [{ role: 'switch', name: 'Follow' }, { text: 'Follow', exact: true }] }, kind: 'box', label: '3', side: 'right' },
    ],
  },
];
