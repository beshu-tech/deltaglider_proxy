// Pure decisions behind the Jobs screen's live state: poll policy after an
// action, Runs refetch, Last run cell, Failures summary, one-off Definition,
// Cancel confirmation, edited-rule detection.
import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  ACTIVE_POLL_MS,
  IDLE_POLL_MS,
  WATCH_WINDOW_MS,
  availableActions,
  cancelConfirmText,
  editedRuleIds,
  failuresCountText,
  groupFailureErrors,
  jobsPollInterval,
  lastRunCell,
  lifecycleVerb,
  nextRunText,
  oneOffDefinition,
  overlayLiveRun,
  phaseWord,
  runMeterInput,
  runsRefreshKey,
  startJobWatch,
  stepJobWatch,
  runEndedMessage,
  statusCellView,
  liveCopySummary,
  jobStatusLabel,
  watchSettled,
  type JobRow,
} from '../jobsView';
import { ruleUid, withRuleUid } from '../ruleUid';

const row = (over: Partial<JobRow> = {}): JobRow => ({
  id: 'replication:r1',
  kind: 'replication',
  name: 'r1',
  scope: { bucket: 'releases', target: 'downloads' },
  trigger: 'continuous',
  enabled: true,
  paused: false,
  status: 'idle',
  status_raw: 'idle',
  progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
  detail: {},
  ...over,
});

// Regression: Apply / Resume made the scheduler start a run 0.3 s later, but
// the list went back to its idle poll at once and the run was never seen.
test('a watch polls fast until the run is seen and settles', () => {
  const t0 = 1_000_000;
  const w = startJobWatch('replication:r1', row({ last_run_at: 100 }), t0);
  assert.equal(jobsPollInterval([row()], [w]), ACTIVE_POLL_MS, 'fast while watching');
  assert.equal(jobsPollInterval([row()], []), IDLE_POLL_MS);

  // Not started yet: keep watching.
  assert.equal(stepJobWatch(w, row({ last_run_at: 100 }), t0 + 1000), w);
  // Started: still watching, now it has seen the run.
  const seen = stepJobWatch(w, row({ status: 'running', last_run_at: 100 }), t0 + 2000);
  assert.equal(seen?.sawActive, true);
  // Settled: done.
  assert.equal(stepJobWatch(seen!, row({ status: 'succeeded', last_run_at: 105 }), t0 + 4000), null);
});

test('a watch also ends on a run that finished between two polls, or at the deadline', () => {
  const t0 = 0;
  const w = startJobWatch('replication:r1', row({ last_run_at: 100 }), t0);
  assert.equal(stepJobWatch(w, row({ last_run_at: 101 }), t0 + 1000), null, 'last_run_at moved');
  assert.equal(stepJobWatch(w, row({ last_run_at: 100 }), t0 + WATCH_WINDOW_MS + 1), null, 'no run came');
  // A new rule is not in the list yet right after Apply: wait for it.
  assert.equal(stepJobWatch(w, undefined, t0 + 1000), w);
});

// Regression: the window was exactly ONE scheduler check (30 s), so a rule
// whose run started at that check was no longer watched: the list dropped to
// its idle poll and a short run was never seen while it ran.
test('a watch outlives one scheduler check, so the first run is still watched', () => {
  const t0 = 0;
  const w = startJobWatch('replication:r1', row({ last_run_at: 100 }), t0);
  assert.ok(WATCH_WINDOW_MS > 30_000, 'longer than the scheduler check interval');
  const still = stepJobWatch(w, row({ last_run_at: 100 }), t0 + 30_000);
  assert.equal(still && still.id, w.id, 'the check that starts the run is inside the window');
  assert.equal(jobsPollInterval([row()], still ? [still] : []), ACTIVE_POLL_MS, 'still polling fast');
  assert.equal(stepJobWatch(still!, row({ status: 'running', last_run_at: 100 }), t0 + 30_500)?.sawActive, true);
});

// Regression: the Runs tab refetched only on active→terminal, so a run the
// scheduler started and finished between polls stayed "No runs yet".
test('runsRefreshKey changes when a run starts, settles, or the rule pauses', () => {
  const idle = runsRefreshKey(row({ last_run_at: 100 }));
  assert.notEqual(runsRefreshKey(row({ last_run_at: 105 })), idle, 'a run finished between polls');
  assert.notEqual(runsRefreshKey(row({ status: 'running', last_run_at: 100 })), idle, 'a run started');
  assert.notEqual(runsRefreshKey(row({ paused: true, last_run_at: 100 })), idle);
  assert.equal(runsRefreshKey(row({ last_run_at: 100, progress: { processed: 9, bytes: 0, failed: 0, skipped: 0 } })), idle, 'progress alone does not refetch');
  assert.equal(runsRefreshKey(null), '');
});

test('overlayLiveRun lays the live numbers on the running run (rule rows have no started_at)', () => {
  const runs = [
    { id: 2, started_at: 50, status: 'running', objects_scanned: 0, objects_processed: 0, objects_skipped: 0, errors: 0, bytes: 0 },
    { id: 1, started_at: 10, status: 'succeeded', objects_scanned: 5, objects_processed: 5, objects_skipped: 0, errors: 0, bytes: 9 },
  ];
  const live = row({ status: 'running', percent: 40, progress: { processed: 7, skipped: 3, failed: 1, bytes: 70, deleted: 2 } });
  const out = overlayLiveRun(runs, live);
  assert.equal(out[0].objects_processed, 7);
  assert.equal(out[0].objects_scanned, 11);
  assert.equal((out[0] as { objects_deleted?: number }).objects_deleted, 2);
  assert.equal(out[0].__percent, 40);
  assert.equal(out[1], runs[1], 'finished runs untouched');
  assert.equal(overlayLiveRun(runs, row()), runs, 'idle row: nothing to overlay');
});

// Regression: "Last run" showed the PREVIOUS run's time during a run.
test('lastRunCell: a run in progress shows how long it runs; else the last finish', () => {
  const now = 1000;
  assert.deepEqual(lastRunCell(row({ status: 'running', started_at: 940, last_run_at: 500 }), now), {
    running: 'running for 1m',
    finishedAt: 500,
  });
  assert.deepEqual(lastRunCell(row({ status: 'succeeded', last_run_at: 500 }), now), { running: null, finishedAt: 500 });
  const oneoff = row({ trigger: 'oneoff', kind: 'migrate', started_at: 990, finished_at: null, status: 'running' });
  assert.deepEqual(lastRunCell(oneoff, now), { running: 'running for 10s', finishedAt: null });
  assert.deepEqual(lastRunCell({ ...oneoff, status: 'succeeded', finished_at: 995 }, now), {
    running: null,
    finishedAt: 995,
  });
  assert.equal(lastRunCell(row({ status: 'queued', trigger: 'oneoff' }), now).running, 'not started yet');
  assert.equal(lastRunCell(row({ status: 'running' }), now).running, 'running now', 'server without started_at');
});

test('nextRunText: scheduled rules only, and only when they will run', () => {
  const now = 0;
  const due = row({ kind: 'lifecycle', trigger: 'scheduled', next_due_at: 40 * 60 });
  assert.equal(nextRunText(due, now), 'next run in ~40 min');
  assert.equal(nextRunText({ ...due, next_due_at: 0 }, now), null);
  assert.equal(nextRunText({ ...due, paused: true }, now), null);
  assert.equal(nextRunText({ ...due, enabled: false }, now), null);
  assert.equal(nextRunText({ ...due, status: 'running' }, now), null);
  assert.equal(nextRunText(due, now, false), null, 'scheduler switch off');
  assert.equal(nextRunText({ ...due, next_due_at: -5 }, now), 'next run: due now');
  assert.equal(nextRunText(row({ trigger: 'oneoff', next_due_at: 99 }), now), null);
});

test('failures: "N most recent of M" and repeated errors folded', () => {
  assert.equal(failuresCountText(20, 1000), 'The 20 most recent of 1,000 failures.');
  assert.equal(failuresCountText(3, 3), null);
  assert.equal(failuresCountText(3, undefined), null, 'older server: no total');
  assert.deepEqual(
    groupFailureErrors([{ error: 'timeout' }, { error: 'denied' }, { error: 'timeout' }, { error: 'timeout' }]),
    [{ error: 'timeout', count: 3 }],
  );
  assert.deepEqual(groupFailureErrors([{ error: 'a' }, { error: 'b' }]), []);
});

// Regression: a one-off's Definition showed only bucket + raw phase.
test('oneOffDefinition: parameters, counts, bytes, duration', () => {
  const mig = row({
    id: 'maintenance:7',
    kind: 'migrate',
    trigger: 'oneoff',
    status: 'succeeded',
    phase: 'cleanup',
    scope: { bucket: 'releases', target: 'aws-dr' },
    created_at: 100,
    started_at: 100,
    finished_at: 160,
    progress: { processed: 4000, skipped: 0, failed: 0, bytes: 2 * 1024 * 1024 },
    detail: { from_backend: 'hetzner-fsn1', target_backend: 'aws-dr', delete_source: false, target: 'empty' },
  });
  const d = Object.fromEntries(oneOffDefinition(mig, 200));
  assert.equal(d['From backend'], 'hetzner-fsn1');
  assert.equal(d['To backend'], 'aws-dr');
  assert.equal(d['Source copy'], 'kept after the switch');
  assert.equal(d['Objects'], '4,000 copied · 0 already on the target · 0 failed');
  assert.equal(d['Data'], '2.0 MiB');
  assert.equal(d['Took'], '1m');
  assert.equal(d['Phase'], undefined, 'a finished job shows no raw phase');

  const bf = row({
    kind: 'backfill-metadata',
    trigger: 'oneoff',
    status: 'running',
    phase: 'counting',
    started_at: 190,
    detail: { refresh_last_modified: true },
  });
  const b = Object.fromEntries(oneOffDefinition(bf, 200));
  assert.equal(b['Last-Modified'], 'set to the time of the backfill');
  assert.equal(b['Phase'], 'counting objects');
  assert.equal(b['Running for'], '10s');
});

// Cancel of a migration before the switch deletes the staged copies: the
// confirmation must say so.
test('cancelConfirmText names what Cancel throws away', () => {
  const pre = cancelConfirmText({ kind: 'migrate', phase: 'copy', scope: { bucket: 'releases' } });
  assert.match(pre.title, /migration of "releases"/);
  assert.match(pre.content, /deletes the copies/);
  const post = cancelConfirmText({ kind: 'migrate', phase: 'cleanup', scope: { bucket: 'releases' } });
  assert.doesNotMatch(post.content, /deletes the copies/);
  assert.match(cancelConfirmText({ kind: 'reencrypt', phase: 'objects', scope: { bucket: 'b' } }).title, /re-encrypt job/);
});

test('availableActions: cancel only while it can still act; no Run now while a run stops', () => {
  const oneoff = row({ kind: 'migrate', trigger: 'oneoff' });
  assert.deepEqual(availableActions({ ...oneoff, status: 'running' }), ['cancel']);
  assert.deepEqual(availableActions({ ...oneoff, status: 'cancelling' }), []);
  // A finished one-off can be removed from the list.
  assert.deepEqual(availableActions({ ...oneoff, status: 'succeeded' }), ['delete']);
  // Regression: while Kill was cancelling the run, Run now was already offered.
  assert.equal(availableActions(row({ status: 'cancelling' })).includes('run-now'), false);
  assert.equal(availableActions(row({ status: 'queued' })).includes('run-now'), false);
});

test('editedRuleIds: changed and new rules only, key order ignored', () => {
  const saved = [
    withRuleUid({ name: 'a', source: { bucket: 'x', prefix: '' } }),
    withRuleUid({ name: 'b', bucket: 'y' }),
  ];
  const c = withRuleUid({ name: 'c', bucket: 'w' });
  const now = [{ ...saved[0], source: { prefix: '', bucket: 'x' } }, { ...saved[1], bucket: 'z' }, c];
  assert.deepEqual([...editedRuleIds('lifecycle', now, saved)].sort(), [`lifecycle:#${ruleUid(c)}`, 'lifecycle:b']);
  assert.equal(editedRuleIds('replication', saved, saved).size, 0);
});

test('phaseWord and lifecycleVerb', () => {
  assert.equal(phaseWord('migrate', 'stage'), 'preparing');
  assert.equal(phaseWord('migrate', 'copy'), 'copying');
  assert.equal(phaseWord('migrate', 'flip'), 'switching to the new backend');
  assert.equal(phaseWord('reencrypt', 'objects'), null, 'the plain walk has no word');
  assert.equal(phaseWord('reencrypt', 'counting'), 'counting objects');
  assert.equal(lifecycleVerb('delete'), 'delete');
  assert.equal(lifecycleVerb({ type: 'transition', destination: { bucket: 'd' }, delete_source_after_success: true }), 'move');
  assert.equal(lifecycleVerb({ type: 'transition', destination: { bucket: 'd' } }), 'copy');
  assert.equal(lifecycleVerb({ type: 'retain-newest', count: 3 }), 'delete');
  assert.equal(lifecycleVerb('transition'), undefined, 'row detail alone does not say');
});

// Regression: "6 scanned, 6 copied, 1 error" — failed objects were left out
// of the scanned count.
test('runMeterInput: scanned is never below its parts', () => {
  const inp = runMeterInput(
    { objects_scanned: 6, objects_processed: 6, objects_skipped: 0, errors: 1, status: 'succeeded' },
    'backfill-metadata',
  );
  assert.equal(inp.scanned, 7);
  assert.equal(inp.kind, 'backfill-metadata');
});

// Lifecycle #15: no word after a Run now ended.
test('runEndedMessage says how a run the operator started ended', () => {
  const lc = row({ id: 'lifecycle:x', kind: 'lifecycle', name: 'expire', trigger: 'scheduled' });
  assert.deepEqual(runEndedMessage({ ...lc, status: 'succeeded' }), {
    type: 'success',
    text: 'Lifecycle "expire": the run finished — the Runs tab has its counts',
  });
  assert.equal(runEndedMessage({ ...lc, status: 'failed', last_error: 'bucket gone' }).text, 'Lifecycle "expire": the run failed — bucket gone');
  assert.equal(runEndedMessage({ ...lc, status: 'completed_with_errors' }).type, 'warning');
  assert.equal(
    runEndedMessage(
      { ...lc, status: 'succeeded', progress: { processed: 3, skipped: 0, failed: 0, bytes: 0 } },
      'delete',
    ).text,
    'Lifecycle "expire": the run finished — 3 deleted',
  );
  const w = startJobWatch('lifecycle:x', { last_run_at: 5 }, 0, true);
  assert.equal(w.announce, true);
  assert.equal(watchSettled(w, { status: 'running', last_run_at: 5 }), false);
  assert.equal(watchSettled(w, { status: 'succeeded', last_run_at: 9 }), true);
  assert.equal(watchSettled(w, { status: 'succeeded', last_run_at: 5 }), false, 'nothing ran yet');
});

// REPL: a settled rule row showed only "succeeded", not what its last run did.
test('statusCellView: a settled rule keeps the counts of its last run', () => {
  const counts = { processed: 14, skipped: 3, failed: 0, bytes: 0, deleted: 1 };
  assert.deepEqual(statusCellView(row({ status: 'succeeded', progress: counts })), { meter: true, tag: true });
  assert.deepEqual(statusCellView(row({ status: 'succeeded' })), { meter: false, tag: true }, 'no run yet');
  assert.deepEqual(statusCellView(row({ status: 'succeeded', paused: true, progress: counts })), { meter: true, tag: true });
  assert.deepEqual(statusCellView(row({ status: 'paused', progress: counts })), { meter: true, tag: true });
  assert.deepEqual(statusCellView(row({ status: 'running' })), { meter: true, tag: false });
  assert.deepEqual(statusCellView(row({ trigger: 'oneoff', status: 'succeeded' })), { meter: true, tag: false });
  assert.deepEqual(statusCellView(row(), true), { meter: false, tag: true }, 'draft');
});

// REPL: a run that a pause stopped kept its position; it is not "cancelled".
test('a stopped (paused) run has its own label', () => {
  assert.equal(jobStatusLabel(row({ status: 'paused' })), 'stopped · resumes where it left off');
  assert.equal(jobStatusLabel(row({ status: 'paused', paused: true })), 'paused');
});

// REPL: event-driven copies were invisible on the row.
test('liveCopySummary: when the last live copy landed, and the counts', () => {
  assert.deepEqual(
    liveCopySummary(row({ detail: { last_event_applied_at: 500, events: { copied: 14, deleted: 1, failed: 0 } } })),
    { at: 500, counts: '14 copied · 1 deleted' },
  );
  assert.deepEqual(liveCopySummary(row({ detail: { events: { last_applied_at: 7, copied: 0, failed: 2 } } })), {
    at: 7,
    counts: '2 failed',
  });
  assert.equal(liveCopySummary(row({ detail: {} })), null);
  assert.equal(liveCopySummary(row({ detail: { events: null } })), null);
  assert.equal(liveCopySummary(row({ kind: 'lifecycle', detail: { last_event_applied_at: 5 } })), null);
});
