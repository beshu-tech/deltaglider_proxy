/**
 * The Verify tab keeps the operator informed through every state of an
 * audit: the previous verdict stays in view while a re-audit runs, the
 * progress names its phase, an audit that did not finish says so, and the
 * audit buttons are off (with the reason) while a run of the rule copies.
 */
import { screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import type { ParityOutcome } from '../adminApi';
import VerifyTab from '../components/jobs/VerifyTab';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const RULE = 'copyHzToBackup';
const VERIFY = `/_/api/admin/jobs/replication:${RULE}/verify`;

afterEach(() => vi.unstubAllGlobals());

const safe: ParityOutcome = {
  rule_name: RULE, source_bucket: 'releases', dest_bucket: 'db-archive',
  source_objects: 4000, dest_objects: 4000, matched: 4000, missing_on_dest: 0, orphan_on_dest: 0,
  checksum_mismatch: 0, unverifiable: 0, truncated: false, cap_hit: false, unresolved: 0,
  in_sync: true, scanned_at: 1_760_000_000, conflict_policy: 'content-diff', replicate_deletes: true,
  actionable: { rerun_fixes: 0, rerun_conditional: 0, needs_manual: 0, copy_failing: 0 },
  missing_samples: [], orphan_samples: [], mismatch_samples: [], verdict: 'safe',
  verdict_summary: 'All 4,000 objects are present and verified identical.',
};

function serve(status: unknown, ruleStatus = 'idle') {
  const http = mockFetch();
  http.on(
    'GET',
    '/_/api/admin/jobs',
    json({ jobs: [{ id: `replication:${RULE}`, kind: 'replication', name: RULE, status: ruleStatus }] }),
  );
  http.on('GET', VERIFY, json(status));
  return http;
}

test('a re-audit keeps the previous verdict in view and names its phase in objects', async () => {
  serve({
    status: 'running',
    phase: 'listing_destination',
    progress_scanned: 1200,
    progress_total: 4000,
    previous_outcome: safe,
    previous_scanned_at: safe.scanned_at,
  });
  renderWithQuery(<VerifyTab ruleName={RULE} />);
  expect(await screen.findByText('Listing the destination…')).toBeInTheDocument();
  expect(await screen.findByText(/1,200 of about 4,000 objects listed/, {}, { timeout: 3000 })).toBeInTheDocument();
  // The previous verdict, dimmed under the live progress.
  expect(screen.getByText('All 4,000 objects are present and verified identical.')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Cancel' })).toBeInTheDocument();
});

test('a first audit shows its phase without a previous verdict', async () => {
  serve({ status: 'running', phase: 'reading', progress_scanned: 10, progress_total: 40 });
  renderWithQuery(<VerifyTab ruleName={RULE} />);
  expect(await screen.findByText('Reading object metadata…')).toBeInTheDocument();
  expect(await screen.findByText(/10 of 40 objects read/, {}, { timeout: 3000 })).toBeInTheDocument();
});

test('a cancelled audit says so above the last complete result', async () => {
  serve({ status: 'cancelled', progress_scanned: 0, progress_total: 0, outcome: safe });
  renderWithQuery(<VerifyTab ruleName={RULE} />);
  expect(await screen.findByText('The last audit was cancelled before it finished')).toBeInTheDocument();
  expect(screen.getByText('Verified in sync')).toBeInTheDocument();
});

test('a failed audit shows the server error, not a generic hint', async () => {
  serve({
    status: 'failed',
    progress_scanned: 0,
    progress_total: 0,
    outcome: safe,
    error: 'list releases/ page failed: connection refused',
  });
  renderWithQuery(<VerifyTab ruleName={RULE} />);
  expect(await screen.findByText('The last audit failed')).toBeInTheDocument();
  expect(screen.getByText('list releases/ page failed: connection refused')).toBeInTheDocument();
});

test('while a run of the rule copies, the audit button is off and says why', async () => {
  serve({ status: 'done', progress_scanned: 0, progress_total: 0, outcome: safe }, 'running');
  renderWithQuery(<VerifyTab ruleName={RULE} />);
  const button = await screen.findByRole('button', { name: /Re-audit/ });
  expect(await screen.findByRole('note')).toHaveTextContent(/run of this rule is in progress/);
  expect(button).toBeDisabled();
});
