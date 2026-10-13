/**
 * "Run now" in the verification's Differences table runs the WHOLE rule, so
 * it is one button for the table. It was a button on every row, and a click
 * on one put every row's button into the loading state.
 */
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import type { ParityFinding, ParityOutcome } from '../adminApi';
import { ParityResult } from '../components/jobs/VerifyTab';
import { useColors } from '../ThemeContext';

const missing = (key: string): ParityFinding => ({
  key,
  kind: 'missing_on_dest',
  unverifiable: false,
  detail: 'present on source, never copied to destination',
  remediation: {
    reason: 'never_copied',
    rerun_helps: { verdict: 'yes' },
    fix: { action: 'run_now' },
    reason_detail: 'present on source, never copied to destination',
    fix_detail: 're-run the rule — a missing key is always copied',
  },
});

const outcome: ParityOutcome = {
  rule_name: 'copyHzToBackup', source_bucket: 'releases', dest_bucket: 'db-archive',
  source_objects: 10, dest_objects: 7, matched: 7, missing_on_dest: 3, orphan_on_dest: 0,
  checksum_mismatch: 0, unverifiable: 0, truncated: false, in_sync: false, scanned_at: 1_760_000_000,
  conflict_policy: 'content-diff', replicate_deletes: true,
  actionable: { rerun_fixes: 3, rerun_conditional: 0, needs_manual: 0, copy_failing: 0 },
  missing_samples: [missing('a.zip'), missing('b.zip'), missing('c.zip')],
  orphan_samples: [], mismatch_samples: [],
};

function Harness(props: { onRunNow: () => void; runNowPending?: boolean }) {
  const c = useColors();
  return <ParityResult outcome={outcome} onReverify={() => {}} c={c} {...props} />;
}

test('the Differences table has one Run now, which runs the rule once', async () => {
  const onRunNow = vi.fn();
  render(<Harness onRunNow={onRunNow} />);
  const buttons = screen.getAllByRole('button', { name: /Run now/ });
  expect(buttons).toHaveLength(1);
  expect(screen.getByText('A run of the rule fixes 3 of the 3 shown')).toBeInTheDocument();
  expect(screen.getAllByText('Run the rule')).toHaveLength(3);
  await userEvent.setup().click(buttons[0]);
  expect(onRunNow).toHaveBeenCalledTimes(1);
});

test('while the run starts, only that one button shows it', () => {
  render(<Harness onRunNow={() => {}} runNowPending />);
  const table = screen.getByText('Differences').parentElement?.parentElement as HTMLElement;
  expect(within(table).getAllByRole('button', { name: /Run now/ })).toHaveLength(1);
});
