/**
 * Observability shots: the rule tester, the audit log, compression health
 * (docs: how-to/trace-requests, how-to/gate-requests-with-admission-rules).
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
];
