/**
 * When an audit could not read some objects, the Verify tab offers to
 * re-check only those objects. A full audit re-lists and re-reads the whole
 * mirror, which is slow on a big bucket.
 */
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi } from 'vitest';
import type { ParityOutcome } from '../adminApi';
import VerifyTab from '../components/jobs/VerifyTab';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const VERIFY = '/_/api/admin/jobs/replication:copyHzToBackup/verify';

afterEach(() => vi.unstubAllGlobals());

function partial(unresolved: number, kept: number, rechecked_at?: number, error?: string): ParityOutcome {
  return {
    rule_name: 'copyHzToBackup', source_bucket: 'releases', dest_bucket: 'db-archive',
    source_objects: 100, dest_objects: 100, matched: 100, missing_on_dest: 0, orphan_on_dest: 0,
    checksum_mismatch: 0, unverifiable: 0, truncated: true, cap_hit: false, unresolved,
    unresolved_keys: Array.from({ length: kept }, (_, i) => ({ dest_key: `k${i}.zip`, source_key: `k${i}.zip`, error })),
    unresolved_keys_capped: kept < unresolved, rechecked_at,
    in_sync: false, scanned_at: 1_760_000_000, conflict_policy: 'content-diff', replicate_deletes: true,
    actionable: { rerun_fixes: 0, rerun_conditional: 0, needs_manual: 0, copy_failing: 0 },
    missing_samples: [], orphan_samples: [], mismatch_samples: [], verdict: 'incomplete',
    verdict_summary: `Nothing is missing or different, but ${unresolved} object(s) couldn't be read.`,
  };
}

function serve(outcome: ParityOutcome) {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/jobs', json({ jobs: [] }));
  http.on('GET', VERIFY, json({ status: 'done', progress_scanned: 0, progress_total: 0, outcome }));
  http.on('POST', VERIFY, json({ status: 'running', progress_scanned: 0, progress_total: 36 }, 202));
  return http;
}

test('the banner re-checks only the objects that could not be read', async () => {
  const http = serve(partial(18, 18));
  renderWithQuery(<VerifyTab ruleName="copyHzToBackup" />);
  const button = await screen.findByRole('button', { name: /Re-check these 18 objects/ });
  // The full audit stays available as its own button.
  expect(screen.getByRole('button', { name: /Audit again/ })).toBeInTheDocument();
  // The concurrency hint waits until a re-check also fails.
  expect(screen.queryByText('DGP_PARITY_HEAD_CONCURRENCY')).toBeNull();
  await userEvent.setup().click(button);
  await waitFor(() => expect(http.callsTo('POST', VERIFY)).toHaveLength(1));
  expect(http.callsTo('POST', VERIFY)[0].path).toBe(`${VERIFY}?scope=unresolved`);
});

test('a capped list says how many objects the re-check reads', async () => {
  serve(partial(1500, 1000));
  renderWithQuery(<VerifyTab ruleName="copyHzToBackup" />);
  expect(await screen.findByRole('button', { name: /Re-check these 1,000 objects/ })).toBeInTheDocument();
  expect(screen.getByText(/A full audit reads the other\s+500/)).toBeInTheDocument();
});

test('after a re-check that the backend throttled again, the banner names the concurrency setting', async () => {
  serve(partial(3, 3, 1_760_000_600, 'source: service error: 503 SlowDown'));
  renderWithQuery(<VerifyTab ruleName="copyHzToBackup" />);
  expect(await screen.findByRole('button', { name: /Re-check these 3 objects/ })).toBeInTheDocument();
  expect(screen.getByText('DGP_PARITY_HEAD_CONCURRENCY')).toBeInTheDocument();
  expect(screen.getByText(/unread objects re-checked/)).toBeInTheDocument();
  expect(screen.getByText('source: service error: 503 SlowDown')).toBeInTheDocument();
});

test('after a re-check that timed out again, the banner shows the error and not the concurrency setting', async () => {
  serve(partial(3, 3, 1_760_000_600, 'destination: operation timed out'));
  renderWithQuery(<VerifyTab ruleName="copyHzToBackup" />);
  expect(await screen.findByText('destination: operation timed out')).toBeInTheDocument();
  expect(screen.getByText(/did not answer in time/)).toBeInTheDocument();
  expect(screen.queryByText('DGP_PARITY_HEAD_CONCURRENCY')).toBeNull();
});

test('the unread objects are listed by name, and the headline does not contradict the body', async () => {
  serve(partial(3, 3));
  renderWithQuery(<VerifyTab ruleName="copyHzToBackup" />);
  const list = await screen.findByRole('list', { name: 'Unread objects' });
  expect(list).toHaveTextContent('k0.zip');
  expect(list).toHaveTextContent('k2.zip');
  // The whole mirror was listed: the headline says no "(scanned portion)".
  expect(screen.getByText('Not fully verified')).toBeInTheDocument();
  expect(screen.queryByText(/scanned portion/)).toBeNull();
  // No sampled differences: no empty Differences table and its advice.
  expect(screen.queryByText('No sampled differences')).toBeNull();
  expect(screen.queryByText(/After fixing, run verification again/)).toBeNull();
});

test('an outcome without the key list offers only the full audit', async () => {
  const legacy = partial(18, 0);
  delete legacy.unresolved_keys;
  serve(legacy);
  renderWithQuery(<VerifyTab ruleName="copyHzToBackup" />);
  expect(await screen.findByText(/only a full audit can read them again/)).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: /Re-check/ })).toBeNull();
});
