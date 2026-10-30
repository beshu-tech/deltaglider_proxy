import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  savedRulesChangedByApply,
  ruleIdsChangedByApply,
  findEditorRule,
  editorRuleJobId,
  editedRuleIds,
  approxTimeUntil,
  parseJobId,
  isActiveJobStatus,
  jobStatusTone,
  jobStatusLabel,
  kindLabel,
  triggerLabel,
  availableActions,
  draftBlocksAction,
  lifecycleRunNowBlock,
  progressLabel,
  busyJobForBucket,
  lastReencryptText,
  mergeDraftRules,
  parityKindMeta,
  conflictPolicyLabel,
  rerunVerdictMeta,
  fixActionMeta,
  computeRate,
  jobWalkProgress,
  jobStrategyMix,
  stepPendingReverify,
  runNowMessage,
  jobsPollInterval,
  runsPollInterval,
  ACTIVE_POLL_MS,
  IDLE_POLL_MS,
  kindTone,
  lifecycleRunLabel,
  type JobRow,
} from '../jobsView';
import { ruleUid, withRuleUid } from '../ruleUid';
import type { ConflictPolicy, FixAction, JobRunEntry } from '../adminApi';

const row = (over: Partial<JobRow> = {}): JobRow => ({
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

test('parseJobId', () => {
  assert.deepEqual(parseJobId('replication:nightly'), { subsystem: 'replication', key: 'nightly' });
  assert.deepEqual(parseJobId('maintenance:42'), { subsystem: 'maintenance', key: '42' });
  assert.equal(parseJobId('nocolon'), null);
  assert.equal(parseJobId('x:'), null);
  assert.equal(parseJobId(':x'), null);
});

test('status helpers', () => {
  for (const s of ['queued', 'running', 'cancelling']) assert.equal(isActiveJobStatus(s), true, s);
  for (const s of ['idle', 'succeeded', 'failed', 'cancelled']) assert.equal(isActiveJobStatus(s), false, s);

  assert.equal(jobStatusTone(row({ status: 'running' })), 'processing');
  assert.equal(jobStatusTone(row({ status: 'failed' })), 'error');
  assert.equal(jobStatusTone(row({ status: 'succeeded' })), 'success');
  assert.equal(jobStatusTone(row({ paused: true, status: 'succeeded' })), 'warning', 'paused wins');
  assert.equal(jobStatusTone(row({ enabled: false, status: 'failed' })), 'default', 'disabled wins');
  assert.equal(jobStatusLabel(row({ paused: true, status: 'idle' })), 'paused');
  assert.equal(jobStatusLabel(row({ enabled: false })), 'disabled');
  // A run in progress wins over pause: the run goes on (regression: the row
  // said "paused" while the run kept copying).
  assert.equal(jobStatusLabel(row({ paused: true, status: 'running' })), 'running · pauses after this run');
  assert.equal(jobStatusTone(row({ paused: true, status: 'running' })), 'processing');
  assert.equal(jobStatusLabel(row({ enabled: false, status: 'running' })), 'running', 'a run-once of a disabled rule');
  // queued and cancelling are not "running".
  assert.equal(jobStatusLabel(row({ status: 'queued' })), 'waiting to start');
  assert.equal(jobStatusLabel(row({ status: 'cancelling' })), 'cancelling');
  assert.notEqual(jobStatusTone(row({ status: 'queued' })), 'processing');
  // A run that the proxy stopped is not "cancelled" and not an operator pause.
  assert.equal(isActiveJobStatus('deferred'), false);
  assert.equal(jobStatusLabel(row({ status: 'deferred' })), 'deferred · continues shortly');
  assert.equal(jobStatusTone(row({ status: 'deferred' })), 'warning');

  assert.equal(kindLabel('reencrypt'), 'Re-encrypt');
  assert.equal(kindLabel('migrate'), 'Migrate');
  assert.equal(triggerLabel('oneoff'), 'one-off');
});

test('draftBlocksAction: preview/run-now never run a stale definition — of THAT rule only', () => {
  const lc = { id: 'lifecycle:expire' };
  const repl = { id: 'replication:mirror' };
  const none = new Set<string>();
  assert.equal(draftBlocksAction('preview', lc, none), null);
  assert.match(draftBlocksAction('preview', lc, new Set(['lifecycle:expire'])) ?? '', /saved rule/);
  assert.match(draftBlocksAction('run-now', repl, new Set(['replication:mirror'])) ?? '', /saved rule/);
  // Regression: one dirty rule blocked Run now on every rule of its kind.
  assert.equal(draftBlocksAction('run-now', repl, new Set(['replication:other'])), null, 'another rule is edited');
  assert.equal(draftBlocksAction('run-now', repl, new Set(['lifecycle:mirror'])), null, 'same name, other kind');
  assert.equal(draftBlocksAction('pause', lc, new Set(['lifecycle:expire'])), null);
});

test('availableActions matrix', () => {
  // A disabled rule that is ALSO paused still offers resume (clears the flag).
  assert.deepEqual(availableActions(row({ enabled: false, paused: true })), ['resume', 'run-now', 'delete']);
  assert.deepEqual(availableActions(row()), ['pause', 'run-now', 'delete']);
  // run-now is a one-off: available even when paused or disabled (backend runs it
  // once without flipping the flag). Only a RUNNING rule has nothing to trigger.
  assert.deepEqual(
    availableActions(row({ paused: true })),
    ['resume', 'run-now', 'delete'],
    'paused still allows a one-off run',
  );
  assert.deepEqual(
    availableActions(row({ status: 'running' })),
    ['pause', 'kill', 'delete'],
    'mid-run: kill available, run-now blocked',
  );
  assert.deepEqual(
    availableActions(row({ enabled: false })),
    ['run-now', 'delete'],
    'disabled still allows a one-off run; pausing a rule that never runs is noise',
  );
  assert.deepEqual(
    availableActions(row({ kind: 'lifecycle' })),
    ['pause', 'preview', 'run-now', 'delete'],
  );
  assert.deepEqual(
    availableActions(row({ kind: 'lifecycle', status: 'running' })),
    ['pause', 'preview', 'kill', 'delete'],
    'a running lifecycle run can be killed (#2)',
  );
  // Lifecycle run-now stays in the list for a paused/disabled rule: the row
  // renders it disabled with the reason from lifecycleRunNowBlock.
  assert.deepEqual(
    availableActions(row({ kind: 'lifecycle', paused: true })),
    ['resume', 'preview', 'run-now', 'delete'],
  );
  assert.deepEqual(
    availableActions(row({ kind: 'lifecycle', enabled: false })),
    ['preview', 'run-now', 'delete'],
  );
  // Replication one-off is unchanged: paused/disabled still runnable.
  assert.deepEqual(
    availableActions(row({ kind: 'replication', paused: true })),
    ['resume', 'run-now', 'delete'],
    'paused replication still allows a one-off',
  );
  assert.deepEqual(
    availableActions(row({ kind: 'reencrypt', trigger: 'oneoff', status: 'running' })),
    ['cancel'],
  );
  assert.deepEqual(
    availableActions(row({ kind: 'migrate', trigger: 'oneoff', status: 'cancelling' })),
    [],
    'cancelling cannot be re-cancelled',
  );
  assert.deepEqual(
    availableActions(row({ kind: 'migrate', trigger: 'oneoff', status: 'succeeded' })),
    ['delete'],
    'a finished one-off can be removed from the list',
  );
  assert.deepEqual(
    availableActions(row({ kind: 'delete', trigger: 'oneoff', status: 'failed' })),
    ['delete'],
  );
});

test('lifecycleRunNowBlock mirrors the server 409s', () => {
  const lc = (over: Partial<JobRow> = {}) => row({ kind: 'lifecycle', ...over });
  assert.equal(lifecycleRunNowBlock(lc(), true), null);
  assert.match(lifecycleRunNowBlock(lc(), false) ?? '', /Run lifecycle rules on schedule/);
  assert.match(lifecycleRunNowBlock(lc({ enabled: false }), true) ?? '', /Enable the rule first/);
  assert.match(lifecycleRunNowBlock(lc({ paused: true }), true) ?? '', /Resume the rule first/);
  // Replication has its one-off: never blocked here.
  assert.equal(lifecycleRunNowBlock(row({ enabled: false, paused: true }), false), null);
});

test('progressLabel', () => {
  assert.equal(
    progressLabel(row({ trigger: 'oneoff', status: 'queued' })),
    'waiting to start…',
  );
  assert.equal(
    progressLabel(
      row({ trigger: 'oneoff', status: 'running', phase: 'objects', progress: { processed: 40, skipped: 10, total: 100, bytes: 0, failed: 0 } }),
    ),
    '50 / 100 objects',
  );
  assert.equal(
    progressLabel(row({ trigger: 'oneoff', status: 'running', phase: 'counting' })),
    'counting objects…',
  );
  // One listing: the total is unknown until the walk ends.
  assert.equal(
    progressLabel(
      row({ trigger: 'oneoff', status: 'running', phase: 'objects', progress: { processed: 40, skipped: 10, total: null, bytes: 0, failed: 0 } }),
    ),
    '50 objects (total known at the end)',
  );
  assert.equal(
    progressLabel(
      row({ trigger: 'oneoff', status: 'running', phase: 'copy', progress: { processed: 5, skipped: 0, total: null, bytes: 0, failed: 0 } }),
    ),
    '5 objects',
  );
  assert.equal(progressLabel(row({ lifetime: { objects: 7, bytes: 1 } })), '7 objects lifetime');
  assert.equal(progressLabel(row()), '—');
});

test('busyJobForBucket', () => {
  const jobs = [
    row({ id: 'maintenance:1', kind: 'reencrypt', trigger: 'oneoff', status: 'running', scope: { bucket: 'PIPPO' } }),
    row({ id: 'maintenance:2', kind: 'migrate', trigger: 'oneoff', status: 'succeeded', scope: { bucket: 'done' } }),
    row({ id: 'replication:r', status: 'running', scope: { bucket: 'pippo' } }),
  ];
  assert.equal(busyJobForBucket(jobs, 'pippo')?.id, 'maintenance:1', 'case-insensitive, one-offs only');
  assert.equal(busyJobForBucket(jobs, 'done'), null, 'terminal one-offs are not busy');
});

test('lastReencryptText: the latest re-encrypt job of the bucket, and whether it is clean', () => {
  const re = (id: number, status: string, created_at: number, extra: Partial<JobRow> = {}) =>
    row({ id: `maintenance:${id}`, kind: 'reencrypt', trigger: 'oneoff', status, created_at, scope: { bucket: 'releases' }, ...extra });
  const progress = (failed: number) => ({ processed: 10, bytes: 0, failed, skipped: 0 });

  assert.deepEqual(lastReencryptText([], 'releases'), { text: 'releases: no recent re-encrypt job.', clean: false });
  // Another kind or bucket does not count.
  const other = [
    row({ id: 'maintenance:1', kind: 'migrate', trigger: 'oneoff', status: 'succeeded', scope: { bucket: 'releases' } }),
    re(2, 'succeeded', 5, { scope: { bucket: 'downloads' } }),
  ];
  assert.equal(lastReencryptText(other, 'releases').clean, false);

  // The newest job wins, whatever its place in the list (case-insensitive).
  const clean = lastReencryptText([re(3, 'failed', 1), re(4, 'succeeded', 9, { progress: progress(0) })], 'RELEASES');
  assert.deepEqual(clean, { text: 'RELEASES: the last re-encrypt job finished with no failures.', clean: true });
  assert.match(lastReencryptText([re(4, 'succeeded', 9), re(3, 'failed', 1)], 'releases').text, /no failures/);

  const cases: Array<[JobRow, RegExp]> = [
    [re(5, 'running', 1), /still running/],
    [re(5, 'queued', 1), /still running/],
    [re(5, 'failed', 1, { last_error: 'backend unreachable' }), /failed \(backend unreachable\)/],
    [re(5, 'cancelled', 1), /cancelled before it finished/],
    [re(5, 'completed_with_errors', 1, { progress: progress(3) }), /finished with 3 failures/],
    [re(5, 'completed_with_errors', 1, { progress: progress(0), failed_total: 1 }), /finished with 1 failure\./],
    [re(5, 'completed_with_errors', 1, { progress: progress(0) }), /finished with errors/],
    [re(5, 'succeeded', 1, { progress: progress(2) }), /finished with 2 failures/],
  ];
  for (const [job, text] of cases) {
    const got = lastReencryptText([job], 'releases');
    assert.match(got.text, text, job.status);
    assert.equal(got.clean, false, job.status);
  }
});

test('mergeDraftRules', () => {
  const server = [
    row({ id: 'replication:keep', name: 'keep' }),
    row({ id: 'replication:gone', name: 'gone' }),
    row({ id: 'lifecycle:lc', kind: 'lifecycle', name: 'lc', trigger: 'scheduled' }),
    row({ id: 'maintenance:9', kind: 'reencrypt', trigger: 'oneoff', name: 'b' }),
  ];
  const keep = withRuleUid({ name: 'keep' });
  const gone = withRuleUid({ name: 'gone' });
  const lcRule = withRuleUid({ name: 'lc' });
  const fresh = withRuleUid({ name: 'fresh' });
  const replSaved = [keep, gone];
  const lcEd = { rules: [lcRule], baseline: [lcRule] };
  const merged = mergeDraftRules(server, { rules: [keep, fresh], baseline: replSaved }, lcEd);
  const byId = Object.fromEntries(merged.map((d) => [d.row.id, d]));
  const freshId = `replication:#${ruleUid(fresh)}`;
  assert.equal(byId['replication:keep'].pendingDelete, false);
  assert.equal(byId['replication:gone'].pendingDelete, true, 'editor-removed rule flagged');
  assert.equal(byId[freshId].draft, true, 'editor-only rule is a draft');
  assert.equal(byId[freshId].row.name, 'fresh');
  assert.equal(byId[freshId].row.status, 'idle');
  // Regression: a draft row showed scope "—" though the editor held a bucket.
  const scoped = mergeDraftRules(
    [],
    { rules: [{ name: 'r', source: { bucket: 'releases', prefix: 'v1' }, destination: { bucket: 'downloads' } }], baseline: [] },
    { rules: [{ name: 'l', bucket: 'db-archive', prefix: '' }], baseline: [] },
  );
  assert.deepEqual(scoped[0].row.scope, { bucket: 'releases', prefix: 'v1', target: 'downloads' });
  assert.deepEqual(scoped[1].row.scope, { bucket: 'db-archive', prefix: undefined });
  // A server rule with unapplied edits carries the marker.
  const marked = mergeDraftRules(server, { rules: [keep], baseline: replSaved }, lcEd, new Set(['replication:keep']));
  assert.equal(marked.find((d) => d.row.id === 'replication:keep')?.edited, true);
  assert.equal(marked.find((d) => d.row.id === 'lifecycle:lc')?.edited, false);
  assert.equal(byId['lifecycle:lc'].pendingDelete, false);
  assert.equal(byId['maintenance:9'].draft, false, 'one-offs pass through');
  assert.equal(byId['maintenance:9'].pendingDelete, false);
});

test('parityKindMeta (Verify tab findings table)', () => {
  assert.deepEqual(parityKindMeta('missing_on_dest'), { label: 'Missing on dest', color: 'gold' });
  assert.deepEqual(parityKindMeta('orphan_on_dest'), { label: 'Extra on dest', color: 'blue' });
  assert.deepEqual(parityKindMeta('checksum_mismatch'), { label: 'Checksum mismatch', color: 'red' });
  assert.deepEqual(parityKindMeta('match'), { label: 'match', color: 'default' }, 'unknown kind falls through');
});

test('conflictPolicyLabel', () => {
  const p = (v: ConflictPolicy) => v;
  assert.equal(conflictPolicyLabel(p('newer-wins')), 'newer wins');
  assert.equal(conflictPolicyLabel(p('content-diff')), 'content diff');
  assert.equal(conflictPolicyLabel(p('skip-if-dest-exists')), 'skip if destination exists');
});

test('rerunVerdictMeta (the policy-aware verdict chip)', () => {
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
    assert.match(m.cause ?? '', /skips existing destination/);
  }
  // dest newer / copy failing — also hard (red) no.
  assert.equal(rerunVerdictMeta({ verdict: 'no', why: 'dest_newer_than_source' }).color, 'red');
  assert.equal(rerunVerdictMeta({ verdict: 'no', why: 'copy_keeps_failing' }).color, 'red');
  // tied timestamps — a distinct, honest cause (not the false "destination is newer").
  assert.match(
    rerunVerdictMeta({ verdict: 'no', why: 'tied_timestamps_no_winner' }).cause ?? '',
    /timestamps tied/,
  );
  // orphan-needs-delete — soft (gold) no: the real fix is enabling mirror-delete.
  assert.equal(rerunVerdictMeta({ verdict: 'no', why: 'orphan_needs_delete' }).color, 'gold');
  const noWhys = [
    'policy_skips_existing_dest',
    'dest_newer_than_source',
    'tied_timestamps_no_winner',
    'orphan_needs_delete',
    'copy_keeps_failing',
  ] as const;
  for (const why of noWhys) {
    assert.equal(rerunVerdictMeta({ verdict: 'no', why }).tone, 'bad', `no:${why} is a bad tone`);
  }
});

test('fixActionMeta (the guided-action affordance)', () => {
  // run_now is the ONLY runnable action.
  assert.deepEqual(fixActionMeta({ action: 'run_now' }), { label: 'Run now', runnable: true });
  // change_conflict_policy → instructional, carries the target policy in the label.
  {
    const m = fixActionMeta({ action: 'change_conflict_policy', to: 'content-diff' });
    assert.equal(m.label, 'Change policy to content-diff');
    assert.equal(m.runnable, false);
    assert.match(m.how ?? '', /conflict policy/);
  }
  // enable_replicate_deletes.
  {
    const m = fixActionMeta({ action: 'enable_replicate_deletes' });
    assert.equal(m.label, 'Enable mirror-delete');
    assert.equal(m.runnable, false);
    assert.match(m.how ?? '', /replicate_deletes/);
  }
  // copy_overwrite.
  {
    const m = fixActionMeta({ action: 'copy_overwrite' });
    assert.equal(m.label, 'Overwrite manually');
    assert.equal(m.runnable, false);
    assert.match(m.how ?? '', /bulk copy/);
  }
  // delete_from_dest — label distinguishes foreign vs ours.
  assert.equal(fixActionMeta({ action: 'delete_from_dest', foreign: true }).label, 'Delete foreign object');
  assert.equal(fixActionMeta({ action: 'delete_from_dest', foreign: false }).label, 'Delete from destination');
  assert.match(fixActionMeta({ action: 'delete_from_dest', foreign: true }).how ?? '', /bulk delete/);
  // resolve_copy_failure — uses the finding's failure detail when given.
  {
    const m = fixActionMeta({ action: 'resolve_copy_failure' }, 'last error: AccessDenied');
    assert.equal(m.label, 'Fix the copy error');
    assert.equal(m.runnable, false);
    assert.equal(m.how, 'last error: AccessDenied');
  }
  assert.match(fixActionMeta({ action: 'resolve_copy_failure' }).how ?? '', /Resolve the underlying copy error/);
  // manual_review — no how-to.
  assert.deepEqual(fixActionMeta({ action: 'manual_review' }), { label: 'Review manually', runnable: false });
  // Only run_now is ever runnable.
  const guidanceOnly: FixAction[] = [
    { action: 'copy_overwrite' },
    { action: 'change_conflict_policy', to: 'newer-wins' },
    { action: 'enable_replicate_deletes' },
    { action: 'delete_from_dest', foreign: false },
    { action: 'resolve_copy_failure' },
    { action: 'manual_review' },
  ];
  for (const fix of guidanceOnly) {
    assert.equal(fixActionMeta(fix).runnable, false, `${fix.action} is guidance-only`);
  }
});

test('computeRate: EMA of objects/sec, holds on Δ=0, no negatives', () => {
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
});

test('jobWalkProgress: live only, defensive on shape', () => {
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
});

test('jobStrategyMix', () => {
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
      mix?.segments.map((s) => [s.key, s.count]),
      [
        ['verbatim', 6],
        ['reconstructed', 1],
        ['straight', 3],
      ],
      'full mix → three segments in order with straight derived',
    );
    assert.equal(mix?.bytesEgressSaved, 2048);
  }
  {
    // All straight copy (no delta counters): only the straight segment.
    const mix = jobStrategyMix({ objects_processed: 4 });
    assert.deepEqual(
      mix?.segments.map((s) => [s.key, s.count]),
      [['straight', 4]],
      'no delta counters → all straight, other segments omitted',
    );
    assert.equal(mix?.bytesEgressSaved, 0);
  }
  {
    // Over-count guard: counters exceed copied → straight floors at 0.
    const mix = jobStrategyMix({ objects_processed: 5, delta_passthrough: 9 });
    assert.equal(
      mix?.segments.find((s) => s.key === 'straight'),
      undefined,
      'over-count → straight is 0 and omitted',
    );
    assert.equal(mix?.segments[0].count, 9, 'verbatim kept as reported');
  }
});

test('rule identity: the editor matches a rule to its saved twin by uid, never by name', () => {
  const nightly = withRuleUid({ name: 'nightly', source: { bucket: 'releases' } });
  const saved = [nightly];
  // A new rule whose name is typed through "nightly" on the way to "nightly-eu".
  const draft = withRuleUid({ name: 'nightly', source: { bucket: '' } });
  const rules = [nightly, draft];
  const draftId = `replication:#${ruleUid(draft)}`;
  assert.equal(editorRuleJobId('replication', draft, saved), draftId);
  assert.equal(editorRuleJobId('replication', nightly, saved), 'replication:nightly');
  // The server id opens the saved rule; the draft id opens the draft.
  assert.equal(findEditorRule('nightly', rules, saved), 0);
  assert.equal(findEditorRule(`#${ruleUid(draft)}`, rules, saved), 1);
  // The saved rule is not "edited" because a draft has its name.
  assert.deepEqual([...editedRuleIds('replication', rules, saved)], [draftId]);
  const server = [row({ id: 'replication:nightly', name: 'nightly' })];
  const none = { rules: [], baseline: [] };
  const merged = mergeDraftRules(server, { rules, baseline: saved }, none, editedRuleIds('replication', rules, saved));
  assert.deepEqual(
    merged.map((d) => [d.row.id, d.row.name, d.draft, d.pendingDelete, !!d.edited]),
    [
      ['replication:nightly', 'nightly', false, false, false],
      [draftId, 'nightly', true, false, false],
    ],
  );
  // A draft with an empty name stays in the table, as "(unnamed)".
  const nameless = { ...draft, name: '' };
  const blank = mergeDraftRules([], { rules: [nameless], baseline: [] }, none);
  assert.deepEqual(blank.map((d) => [d.row.id, d.row.name]), [[draftId, '(unnamed)']]);
  assert.equal(findEditorRule(`#${ruleUid(draft)}`, [nameless], []), 0);
  // A rename is a delete and a create on the server: the saved row is
  // removed, the renamed rule is a draft, and the server id still opens it.
  const renamed = { ...nightly, name: 'nightly-eu' };
  const ren = mergeDraftRules(server, { rules: [renamed], baseline: saved }, none);
  assert.deepEqual(
    ren.map((d) => [d.row.id, d.row.name, d.draft, d.pendingDelete]),
    [
      ['replication:nightly', 'nightly', false, true],
      [`replication:#${ruleUid(nightly)}`, 'nightly-eu', true, false],
    ],
  );
  assert.equal(findEditorRule('nightly', [renamed], saved), 0);
  // An applied rule that the jobs list does not show yet is a draft row
  // under its server id.
  const early = mergeDraftRules([], { rules: saved, baseline: saved }, none);
  assert.deepEqual(early.map((d) => [d.row.id, d.draft]), [['replication:nightly', true]]);
});

test('ruleIdsChangedByApply / savedRulesChangedByApply', () => {
  const a = withRuleUid({ name: 'a', bucket: 'x' });
  const b = withRuleUid({ name: 'b', bucket: 'y' });
  const c = withRuleUid({ name: 'c', bucket: 'z' });
  const saved = [a, b, c];
  const fresh = withRuleUid({ name: 'new', bucket: 'w' });
  // a unchanged, b changed, c renamed, plus a new rule.
  const rules = [a, { ...b, bucket: 'q' }, { ...c, name: 'c2' }, fresh];
  assert.deepEqual(ruleIdsChangedByApply('lifecycle', rules, saved), ['lifecycle:b', 'lifecycle:c2', 'lifecycle:new']);
  assert.deepEqual(savedRulesChangedByApply(rules, saved), ['b', 'c']);
  // A deleted rule; a new rule that takes a saved name is not that rule.
  assert.deepEqual(savedRulesChangedByApply([withRuleUid({ name: 'a', bucket: 'x' }), b, c], saved), ['a']);
});

test('stepPendingReverify (run-now → verify after the run settles)', () => {
  const p0 = { baselineLastRunAt: 100, sawActive: false };
  // Row not loaded yet → keep waiting.
  assert.deepEqual(stepPendingReverify(p0, null), { next: p0, start: false });
  // Still idle, last_run_at unchanged (run not picked up yet) → wait.
  assert.deepEqual(stepPendingReverify(p0, { status: 'idle', last_run_at: 100 }), { next: p0, start: false });
  // Run observed active → remember it, do not start (409 while the lease is held).
  const p1 = stepPendingReverify(p0, { status: 'running', last_run_at: 100 });
  assert.deepEqual(p1, { next: { baselineLastRunAt: 100, sawActive: true }, start: false });
  // active → terminal edge → start verify, clear pending.
  assert.deepEqual(stepPendingReverify(p1.next ?? p0, { status: 'succeeded', last_run_at: 100 }), { next: null, start: true });
  // A fast run that finished between polls (never seen active): last_run_at moved.
  assert.deepEqual(stepPendingReverify(p0, { status: 'succeeded', last_run_at: 250 }), { next: null, start: true });
  // No prior run at all.
  assert.deepEqual(
    stepPendingReverify({ baselineLastRunAt: null, sawActive: false }, { status: 'failed', last_run_at: 5 }),
    { next: null, start: true },
  );
});

test('runNowMessage (run-now is async for both rule kinds → no fake count)', () => {
  // Both kinds answer 202 {status:'running'}: a count would be meaningless.
  const m = runNowMessage('replication', { status: 'running', objects_copied: 0 });
  assert.equal(m, 'Run started — progress shows in the row and the Runs tab');
  assert.equal(/0 objects/.test(m), false);
  assert.equal(
    runNowMessage('lifecycle', { status: 'running', run_id: 7, objects_affected: 0 }),
    'Run started — progress shows in the row and the Runs tab',
  );
  assert.equal(runNowMessage('lifecycle', null), 'Run started — progress shows in the row and the Runs tab');
});

test('runsPollInterval (the Runs tab polls while a run is in flight)', () => {
  const entry = (status: string): Pick<JobRunEntry, 'status'> => ({ status });
  assert.equal(runsPollInterval([entry('running'), entry('succeeded')]), ACTIVE_POLL_MS);
  assert.equal(runsPollInterval([entry('succeeded'), entry('failed')]), false);
  assert.equal(runsPollInterval([]), false);
});

test('jobsPollInterval (idle floor: scheduled runs must still appear)', () => {
  assert.equal(ACTIVE_POLL_MS, 2000);
  // 10 s: a job started in another tab or by the scheduler shows within
  // 10 s (it was 60 s, and short runs were never seen running).
  assert.equal(IDLE_POLL_MS, 10_000);
  assert.equal(jobsPollInterval([row({ status: 'running' })]), ACTIVE_POLL_MS);
  assert.equal(jobsPollInterval([row({ status: 'idle' }), row({ status: 'queued' })]), ACTIVE_POLL_MS);
  // Regression: all idle used to return `false` (never refetch), so a run the
  // scheduler started never showed on an open Jobs page.
  assert.equal(jobsPollInterval([row({ status: 'idle' })]), IDLE_POLL_MS);
  assert.equal(jobsPollInterval([]), IDLE_POLL_MS);
});

test('kindTone', () => {
  assert.equal(kindTone('replication'), 'blue');
  assert.equal(kindTone('lifecycle'), 'purple');
  assert.equal(kindTone('reencrypt'), 'gold');
  assert.equal(kindTone('migrate'), 'gold');
  assert.equal(kindTone('backfill-metadata'), 'gold');
});

test('lifecycleRunLabel names the verb and the count', () => {
  assert.equal(lifecycleRunLabel(2, ['delete', 'delete']), 'Run: delete 2 objects');
  assert.equal(lifecycleRunLabel(1, ['transition']), 'Run: transition 1 object');
  assert.equal(lifecycleRunLabel(3, ['delete', 'transition']), 'Run: act on 3 objects');
  assert.equal(lifecycleRunLabel(0, []), 'Run: nothing to do');
});

test('approxTimeUntil rounds a wait for a reader', () => {
  const now = 1_000_000_000_000;
  const min = 60_000;
  assert.equal(approxTimeUntil(now - min, now), 'now');
  assert.equal(approxTimeUntil(now, now), 'now');
  assert.equal(approxTimeUntil(now + 40 * min, now), '~40 min');
  assert.equal(approxTimeUntil(now + 13 * 60 * min + min, now), '~13 h');
  assert.equal(approxTimeUntil(now + 3 * 24 * 60 * min, now), '~3 days');
});
