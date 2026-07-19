import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/jobsView.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'jobsView.ts',
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
const {
  parseJobId,
  isActiveJobStatus,
  jobStatusTone,
  jobStatusLabel,
  kindLabel,
  triggerLabel,
  availableActions,
  progressLabel,
  busyJobForBucket,
  mergeDraftRules,
  parityKindMeta,
  conflictPolicyLabel,
  rerunVerdictMeta,
  fixActionMeta,
  computeRate,
  deriveVerifyProgress,
  jobWalkProgress,
  jobStrategyMix,
} = await import(moduleUrl);

const row = (over = {}) => ({
  id: 'replication:r1',
  kind: 'replication',
  name: 'r1',
  scope: { bucket: 'src' },
  trigger: 'continuous',
  enabled: true,
  paused: false,
  status: 'idle',
  status_raw: 'idle',
  progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
  detail: {},
  ...over,
});

// ── parseJobId ──────────────────────────────────────────────────────────────
assert.deepEqual(parseJobId('replication:nightly'), { subsystem: 'replication', key: 'nightly' });
assert.deepEqual(parseJobId('maintenance:42'), { subsystem: 'maintenance', key: '42' });
assert.equal(parseJobId('nocolon'), null);
assert.equal(parseJobId('x:'), null);
assert.equal(parseJobId(':x'), null);

// ── status helpers ──────────────────────────────────────────────────────────
for (const s of ['queued', 'running', 'cancelling']) assert.equal(isActiveJobStatus(s), true, s);
for (const s of ['idle', 'succeeded', 'failed', 'cancelled']) assert.equal(isActiveJobStatus(s), false, s);

assert.equal(jobStatusTone(row({ status: 'running' })), 'processing');
assert.equal(jobStatusTone(row({ status: 'failed' })), 'error');
assert.equal(jobStatusTone(row({ status: 'succeeded' })), 'success');
assert.equal(jobStatusTone(row({ paused: true, status: 'succeeded' })), 'warning', 'paused wins');
assert.equal(jobStatusTone(row({ enabled: false, status: 'failed' })), 'default', 'disabled wins');
assert.equal(jobStatusLabel(row({ paused: true, status: 'idle' })), 'paused');
assert.equal(jobStatusLabel(row({ enabled: false })), 'disabled');

assert.equal(kindLabel('reencrypt'), 'Re-encrypt');
assert.equal(kindLabel('migrate'), 'Migrate');
assert.equal(triggerLabel('oneoff'), 'one-off');

// ── availableActions matrix ─────────────────────────────────────────────────
assert.deepEqual(availableActions(row()), ['pause', 'run-now', 'delete']);
// run-now is a one-off: available even when paused or disabled (backend runs it
// once without flipping the flag). Only a RUNNING rule has nothing to trigger.
assert.deepEqual(
  availableActions(row({ paused: true })),
  ['resume', 'run-now', 'delete'],
  'paused still allows a one-off run'
);
assert.deepEqual(
  availableActions(row({ status: 'running' })),
  ['pause', 'kill', 'delete'],
  'mid-run: kill available, run-now blocked'
);
assert.deepEqual(
  availableActions(row({ enabled: false })),
  ['pause', 'run-now', 'delete'],
  'disabled still allows a one-off run'
);
assert.deepEqual(
  availableActions(row({ kind: 'lifecycle' })),
  ['pause', 'preview', 'run-now', 'delete']
);
assert.deepEqual(
  availableActions(row({ kind: 'lifecycle', status: 'running' })),
  ['pause', 'preview', 'delete'],
  'lifecycle has NO kill (backend would 400)'
);
// Lifecycle run-now is NOT a paused/disabled one-off (backend 409s both) — the
// UI must not offer a button the backend structurally rejects.
assert.deepEqual(
  availableActions(row({ kind: 'lifecycle', paused: true })),
  ['resume', 'preview', 'delete'],
  'paused lifecycle: no run-now (backend 409s it)'
);
assert.deepEqual(
  availableActions(row({ kind: 'lifecycle', enabled: false })),
  ['pause', 'preview', 'delete'],
  'disabled lifecycle: no run-now (backend 409s it)'
);
// Replication one-off is unchanged: paused/disabled still runnable.
assert.deepEqual(
  availableActions(row({ kind: 'replication', paused: true })),
  ['resume', 'run-now', 'delete'],
  'paused replication still allows a one-off'
);
assert.deepEqual(
  availableActions(row({ kind: 'reencrypt', trigger: 'oneoff', status: 'running' })),
  ['cancel']
);
assert.deepEqual(
  availableActions(row({ kind: 'migrate', trigger: 'oneoff', status: 'cancelling' })),
  [],
  'cancelling cannot be re-cancelled'
);
assert.deepEqual(
  availableActions(row({ kind: 'migrate', trigger: 'oneoff', status: 'succeeded' })),
  []
);

// ── progressLabel ───────────────────────────────────────────────────────────
assert.equal(
  progressLabel(row({ trigger: 'oneoff', status: 'queued' })),
  'waiting to start…'
);
assert.equal(
  progressLabel(
    row({ trigger: 'oneoff', status: 'running', phase: 'objects', progress: { processed: 40, skipped: 10, total: 100, bytes: 0, failed: 0 } })
  ),
  '50 / 100 objects'
);
assert.equal(
  progressLabel(row({ trigger: 'oneoff', status: 'running', phase: 'counting' })),
  'counting objects…'
);
assert.equal(progressLabel(row({ lifetime: { objects: 7, bytes: 1 } })), '7 objects lifetime');
assert.equal(progressLabel(row()), '—');

// ── busyJobForBucket ────────────────────────────────────────────────────────
const jobs = [
  row({ id: 'maintenance:1', kind: 'reencrypt', trigger: 'oneoff', status: 'running', scope: { bucket: 'PIPPO' } }),
  row({ id: 'maintenance:2', kind: 'migrate', trigger: 'oneoff', status: 'succeeded', scope: { bucket: 'done' } }),
  row({ id: 'replication:r', status: 'running', scope: { bucket: 'pippo' } }),
];
assert.equal(busyJobForBucket(jobs, 'pippo')?.id, 'maintenance:1', 'case-insensitive, one-offs only');
assert.equal(busyJobForBucket(jobs, 'done'), null, 'terminal one-offs are not busy');

// ── mergeDraftRules ─────────────────────────────────────────────────────────
const server = [
  row({ id: 'replication:keep', name: 'keep' }),
  row({ id: 'replication:gone', name: 'gone' }),
  row({ id: 'lifecycle:lc', kind: 'lifecycle', name: 'lc', trigger: 'scheduled' }),
  row({ id: 'maintenance:9', kind: 'reencrypt', trigger: 'oneoff', name: 'b' }),
];
const merged = mergeDraftRules(server, [{ name: 'keep' }, { name: 'fresh' }], [{ name: 'lc' }]);
const byId = Object.fromEntries(merged.map((d) => [d.row.id, d]));
assert.equal(byId['replication:keep'].pendingDelete, false);
assert.equal(byId['replication:gone'].pendingDelete, true, 'editor-removed rule flagged');
assert.equal(byId['replication:fresh'].draft, true, 'editor-only rule is a draft');
assert.equal(byId['replication:fresh'].row.status, 'idle');
assert.equal(byId['lifecycle:lc'].pendingDelete, false);
assert.equal(byId['maintenance:9'].draft, false, 'one-offs pass through');
assert.equal(byId['maintenance:9'].pendingDelete, false);

// ── parityKindMeta (Verify tab findings table) ──────────────────────────────
assert.deepEqual(parityKindMeta('missing_on_dest'), { label: 'Missing on dest', color: 'gold' });
assert.deepEqual(parityKindMeta('orphan_on_dest'), { label: 'Extra on dest', color: 'blue' });
assert.deepEqual(parityKindMeta('checksum_mismatch'), { label: 'Checksum mismatch', color: 'red' });
assert.deepEqual(parityKindMeta('match'), { label: 'match', color: 'default' }, 'unknown kind falls through');

// ── conflictPolicyLabel ─────────────────────────────────────────────────────
assert.equal(conflictPolicyLabel('newer-wins'), 'newer wins');
assert.equal(conflictPolicyLabel('content-diff'), 'content diff');
assert.equal(conflictPolicyLabel('skip-if-dest-exists'), 'skip if destination exists');

// ── rerunVerdictMeta (the policy-aware verdict chip) ────────────────────────
// yes → green/good.
assert.deepEqual(rerunVerdictMeta({ verdict: 'yes' }), {
  label: 'Re-run fixes this',
  color: 'green',
  tone: 'good',
});
// conditional → blue/maybe.
assert.deepEqual(rerunVerdictMeta({ verdict: 'conditional', why: 'newer_wins_depends_on_timestamps' }), {
  label: 'Depends on timestamps',
  color: 'blue',
  tone: 'maybe',
});
// conditional/transient → "Re-run may help" (a stalled/slow read may clear on retry).
assert.deepEqual(
  rerunVerdictMeta({ verdict: 'conditional', why: 'transient_copy_error_may_clear' }),
  { label: 'Re-run may help', cause: 'transient error — retry', color: 'blue', tone: 'maybe' },
);
// THE LIE — skip-if-dest-exists mismatch: a HARD no (red). The verdict label is
// now a fixed short chip; the specific cause moved to `cause` (de-dup fix so the
// WHY column doesn't say the same thing twice).
{
  const m = rerunVerdictMeta({ verdict: 'no', why: 'policy_skips_existing_dest' });
  assert.equal(m.color, 'red', 'policy-skip is a hard (red) no');
  assert.equal(m.tone, 'bad');
  assert.equal(m.label, "Re-run won't help");
  assert.match(m.cause, /skips existing destination/);
}
// dest newer / copy failing — also hard (red) no.
assert.equal(rerunVerdictMeta({ verdict: 'no', why: 'dest_newer_than_source' }).color, 'red');
assert.equal(rerunVerdictMeta({ verdict: 'no', why: 'copy_keeps_failing' }).color, 'red');
// tied timestamps — a distinct, honest cause (not the false "destination is newer").
assert.match(
  rerunVerdictMeta({ verdict: 'no', why: 'tied_timestamps_no_winner' }).cause,
  /timestamps tied/,
);
// orphan-needs-delete / foreign — soft (gold) no: the real fix is an out-of-band delete.
assert.equal(rerunVerdictMeta({ verdict: 'no', why: 'orphan_needs_delete' }).color, 'gold');
assert.equal(rerunVerdictMeta({ verdict: 'no', why: 'foreign_not_ours' }).color, 'gold');
for (const why of ['policy_skips_existing_dest', 'dest_newer_than_source', 'tied_timestamps_no_winner', 'orphan_needs_delete', 'foreign_not_ours', 'copy_keeps_failing']) {
  assert.equal(rerunVerdictMeta({ verdict: 'no', why }).tone, 'bad', `no:${why} is a bad tone`);
}

// ── fixActionMeta (the guided-action affordance) ────────────────────────────
// run_now is the ONLY runnable action.
assert.deepEqual(fixActionMeta({ action: 'run_now' }), { label: 'Run now', runnable: true });
// change_conflict_policy → instructional, carries the target policy in the label.
{
  const m = fixActionMeta({ action: 'change_conflict_policy', to: 'content-diff' });
  assert.equal(m.label, 'Change policy to content-diff');
  assert.equal(m.runnable, false);
  assert.match(m.how, /conflict policy/);
}
// enable_replicate_deletes.
{
  const m = fixActionMeta({ action: 'enable_replicate_deletes' });
  assert.equal(m.label, 'Enable mirror-delete');
  assert.equal(m.runnable, false);
  assert.match(m.how, /replicate_deletes/);
}
// copy_overwrite.
{
  const m = fixActionMeta({ action: 'copy_overwrite' });
  assert.equal(m.label, 'Overwrite manually');
  assert.equal(m.runnable, false);
  assert.match(m.how, /bulk copy/);
}
// delete_from_dest — label distinguishes foreign vs ours.
assert.equal(fixActionMeta({ action: 'delete_from_dest', foreign: true }).label, 'Delete foreign object');
assert.equal(fixActionMeta({ action: 'delete_from_dest', foreign: false }).label, 'Delete from destination');
assert.match(fixActionMeta({ action: 'delete_from_dest', foreign: true }).how, /bulk delete/);
// resolve_copy_failure — uses the finding's failure detail when given.
{
  const m = fixActionMeta({ action: 'resolve_copy_failure' }, 'last error: AccessDenied');
  assert.equal(m.label, 'Fix the copy error');
  assert.equal(m.runnable, false);
  assert.equal(m.how, 'last error: AccessDenied');
}
assert.match(fixActionMeta({ action: 'resolve_copy_failure' }).how, /Resolve the underlying copy error/);
// manual_review — no how-to.
assert.deepEqual(fixActionMeta({ action: 'manual_review' }), { label: 'Review manually', runnable: false });
// Only run_now is ever runnable.
for (const fix of [
  { action: 'copy_overwrite' },
  { action: 'change_conflict_policy', to: 'newer-wins' },
  { action: 'enable_replicate_deletes' },
  { action: 'delete_from_dest', foreign: false },
  { action: 'resolve_copy_failure' },
  { action: 'manual_review' },
]) {
  assert.equal(fixActionMeta(fix).runnable, false, `${fix.action} is guidance-only`);
}

// computeRate: EMA of objects/sec, holds on Δ=0, no negatives ---------------
assert.equal(computeRate(null, 0, 0, 1000, 1000), 1000, 'first sample = instantaneous');
assert.equal(computeRate(1000, 1000, 1000, 1000, 3000), 1000, 'Δ=0 holds previous rate');
assert.equal(computeRate(1000, 5000, 1000, 4000, 3000), 1000, 'backwards count holds previous rate');
assert.equal(computeRate(null, 0, 1000, 0, 1000), 0, 'no dt, no prev → 0');
{
  // 800 obj over 2s = 400/s instant; EMA(0.4) toward 1000 prev = 760
  const r = computeRate(1000, 1000, 1000, 1800, 3000);
  assert.ok(r > 400 && r < 1000, `EMA blends toward instantaneous, got ${r}`);
}
assert.ok(computeRate(5, 100, 1000, 90, 2000) >= 0, 'never negative');

// deriveVerifyProgress: indeterminate until total known, then clamped percent -
assert.deepEqual(deriveVerifyProgress(500, 0), { determinate: false, percent: 0 }, 'total 0 → indeterminate');
assert.deepEqual(deriveVerifyProgress(500, undefined), { determinate: false, percent: 0 }, 'no total → indeterminate');
assert.deepEqual(deriveVerifyProgress(500, 1000), { determinate: true, percent: 50 });
assert.deepEqual(deriveVerifyProgress(2000, 1000), { determinate: true, percent: 100 }, 'clamped at 100');
assert.deepEqual(deriveVerifyProgress(0, 1000), { determinate: true, percent: 0 });

// ── jobWalkProgress: live only, defensive on shape ──────────────────────────
assert.equal(jobWalkProgress(null), null, 'null row → null');
assert.equal(
  jobWalkProgress(row({ status: 'succeeded', detail: { walk: { dirs_completed: 5 } } })),
  null,
  'inactive status → null (no live progress)',
);
assert.equal(
  jobWalkProgress(row({ status: 'running', detail: {} })),
  null,
  'active but no walk detail → null',
);
assert.deepEqual(
  jobWalkProgress(
    row({
      status: 'running',
      detail: { walk: { scanning: 'ror/builds/1.70/', dirs_completed: 157, dirs_pending: 128 } },
    }),
  ),
  { scanning: 'ror/builds/1.70/', dirs_completed: 157, dirs_pending: 128 },
  'active + full walk detail → parsed',
);
assert.deepEqual(
  jobWalkProgress(row({ status: 'running', detail: { walk: { dirs_completed: 3 } } })),
  { scanning: null, dirs_completed: 3, dirs_pending: 0 },
  'partial walk detail → defaults (no scanning, pending 0)',
);
assert.deepEqual(
  jobWalkProgress(row({ status: 'running', detail: { walk: { scanning: 42 } } })),
  { scanning: null, dirs_completed: 0, dirs_pending: 0 },
  'wrong-typed fields → coerced to safe defaults',
);

// --- jobStrategyMix -------------------------------------------------------
assert.equal(jobStrategyMix(null), null, 'null run → null');
assert.equal(jobStrategyMix({ objects_processed: 0 }), null, 'nothing copied → null');
{
  // 10 copied: 6 verbatim + 1 rebuilt → 3 straight (derived).
  const mix = jobStrategyMix({
    objects_processed: 10,
    delta_passthrough: 6,
    reconstructed: 1,
    bytes_egress_saved: 2048,
  });
  assert.deepEqual(
    mix.segments.map((s) => [s.key, s.count]),
    [
      ['verbatim', 6],
      ['reconstructed', 1],
      ['straight', 3],
    ],
    'full mix → three segments in order with straight derived',
  );
  assert.equal(mix.bytesEgressSaved, 2048);
}
{
  // All straight copy (no delta counters): only the straight segment.
  const mix = jobStrategyMix({ objects_processed: 4 });
  assert.deepEqual(
    mix.segments.map((s) => [s.key, s.count]),
    [['straight', 4]],
    'no delta counters → all straight, other segments omitted',
  );
  assert.equal(mix.bytesEgressSaved, 0);
}
{
  // Over-count guard: counters exceed copied → straight floors at 0.
  const mix = jobStrategyMix({ objects_processed: 5, delta_passthrough: 9 });
  assert.equal(
    mix.segments.find((s) => s.key === 'straight'),
    undefined,
    'over-count → straight is 0 and omitted',
  );
  assert.equal(mix.segments[0].count, 9, 'verbatim kept as reported');
}

console.log('jobs view regression checks passed');
