import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  buildBucketPayload,
  freshId,
  policyToRow,
  isAllDefaultRow,
  DEFAULT_ROW_FIELDS,
  type BucketPolicyRow,
} from '../components/bucketPolicyPayload';
import {
  buildLifecyclePayload,
  DEFAULT_LIFECYCLE,
  normalizeLifecycle,
  actionKind,
} from '../components/lifecyclePayload';
import {
  buildReplicationPayload,
  DEFAULT_REPLICATION,
  normalizeReplication,
} from '../components/replicationPayload';
import type { LifecycleConfig, LifecycleRuleConfig, ReplicationConfig, ReplicationRuleConfig } from '../adminApi';

/** Narrow a `{ ok: true; body } | { ok: false; error }` result to its failure branch. */
function failureError<TBody>(res: { ok: true; body: TBody } | { ok: false; error: string }): string {
  if (res.ok) throw new Error('expected a failure result');
  return res.error;
}

// ───────────────────────────────────────────────────────────────────
// BucketsPanel — buildBucketPayload
//
// The storage-section PUT deep-merges (RFC 7396): ABSENT keys preserve
// server values, explicit `null` deletes. The builder therefore emits
// every clearable field explicitly, and takes the BASELINE bucket names
// so removed/reset policies serialise as `name: null`. The pre-2026-06
// builder omitted unset fields — which made un-publicking, un-routing,
// and policy deletion silent server-side no-ops.
// ───────────────────────────────────────────────────────────────────
const defaultFields = DEFAULT_ROW_FIELDS;

test('synthetic ids are unique + monotonic, and NEVER appear in the wire', () => {
  const a = freshId();
  const b = freshId();
  assert.notEqual(a, b, 'freshId must be collision-free');
  assert.ok(a.startsWith('bkt-') && b.startsWith('bkt-'));
});

test('empty-name rows are dropped; a populated row serialises every clearable field explicitly', () => {
  const rows: BucketPolicyRow[] = [
    { _id: 'bkt-x', name: '', ...defaultFields },
    {
      _id: 'bkt-y',
      name: 'prod',
      compression: false,
      max_delta_ratio: 0.5,
      backend: 'b1',
      alias: 'realprod',
      publicMode: 'none',
      public_prefixes: [],
      quota_bytes: 1073741824,
      replication_target_only: false,
    },
  ];
  const res = buildBucketPayload(rows);
  assert.equal(res.ok, true);
  if (!res.ok) return;
  assert.deepEqual(Object.keys(res.body.buckets), ['prod'], 'unnamed row dropped');
  const p = res.body.buckets.prod;
  assert.ok(!('_id' in (p as object)), 'synthetic id must never reach the wire');
  assert.deepEqual(p, {
    compression: false,
    max_delta_ratio: 0.5,
    backend: 'b1',
    alias: 'realprod',
    public: null,
    public_prefixes: null,
    quota_bytes: 1073741824,
    replication_target_only: null,
  });
});

test('replication_target_only passthrough: a MARKER-ONLY policy must not round-trip as "all default"', () => {
  // (that would serialise as policy DELETION and silently wipe the marker on any bucket-panel apply)
  const row = policyToRow('mirror', { replication_target_only: true });
  assert.equal(row.replication_target_only, true);
  assert.equal(isAllDefaultRow(row), false, 'marker-only row is NOT all-default');
  const res = buildBucketPayload([row], ['mirror']);
  assert.equal(res.ok, true);
  if (!res.ok) return;
  assert.equal(
    res.body.buckets.mirror?.replication_target_only,
    true,
    'marker must survive the round-trip verbatim',
  );
  // Unmarked rows emit explicit null so a cleared marker merge-deletes.
  const un = buildBucketPayload([
    { _id: 'bkt-u', name: 'u', ...defaultFields, backend: 'b1' },
  ]);
  assert.equal(un.ok, true);
  if (!un.ok) return;
  assert.equal(un.body.buckets.u?.replication_target_only, null);
});

test('compression:null is preserved as explicit null (merge-clears the key)', () => {
  const res = buildBucketPayload([
    { _id: 'bkt-1', name: 'b', ...defaultFields, backend: 'b1' },
  ]);
  assert.equal(res.ok, true);
  if (!res.ok) return;
  assert.equal(res.body.buckets.b?.compression, null);
  const json = JSON.stringify(res.body);
  assert.ok(json.includes('"compression":null'), 'null compression must survive stringify');
});

test('tri-state public mode → wire sentinels', () => {
  // `entire` => [""]; `prefixes` drops blanks; `none` emits EXPLICIT nulls so
  // the merge clears any previous public state (omission would keep the
  // bucket public).
  const entire = buildBucketPayload([
    { _id: 'bkt-e', name: 'e', ...defaultFields, publicMode: 'entire' },
  ]);
  assert.equal(entire.ok, true);
  if (!entire.ok) return;
  assert.deepEqual(entire.body.buckets.e?.public_prefixes, ['']);
  assert.equal(entire.body.buckets.e?.public, null, 'shorthand `public` always cleared; [""] is the wire spelling');

  const prefixes = buildBucketPayload([
    {
      _id: 'bkt-p', name: 'p', ...defaultFields, publicMode: 'prefixes',
      public_prefixes: [
        { id: 'a', value: 'builds/' },
        { id: 'b', value: '  ' }, // blank dropped
        { id: 'c', value: 'rel/' },
      ],
    },
  ]);
  assert.equal(prefixes.ok, true);
  if (!prefixes.ok) return;
  assert.deepEqual(prefixes.body.buckets.p?.public_prefixes, ['builds/', 'rel/']);

  const none = buildBucketPayload([
    { _id: 'bkt-n', name: 'n', ...defaultFields, backend: 'b1' },
  ]);
  assert.equal(none.ok, true);
  if (!none.ok) return;
  assert.equal(none.body.buckets.n?.public_prefixes, null, 'none emits explicit null (merge-clears)');
  assert.equal(none.body.buckets.n?.public, null);
});

test('duplicate bucket names abort with an error, zero body', () => {
  const res = buildBucketPayload([
    { _id: '1', name: 'dup', ...defaultFields },
    { _id: '2', name: 'dup', ...defaultFields },
  ]);
  assert.equal(res.ok, false);
  if (res.ok) return;
  assert.equal(res.error, 'Duplicate bucket name: dup');
});

test('policyToRow round-trip through buildBucketPayload: public:true shorthand decodes to entire', () => {
  const row = policyToRow('shorthand', { public: true });
  assert.equal(row.publicMode, 'entire');
  const res = buildBucketPayload([row]);
  assert.equal(res.ok, true);
  if (!res.ok) return;
  assert.deepEqual(res.body.buckets.shorthand?.public_prefixes, ['']);
  const row2 = policyToRow('expanded', { public_prefixes: [''] });
  assert.equal(row2.publicMode, 'entire');
  const res2 = buildBucketPayload([row2]);
  assert.equal(res2.ok, true);
  if (!res2.ok) return;
  assert.deepEqual(res2.body.buckets.expanded?.public_prefixes, ['']);
});

test('isAllDefaultRow truth table — incl. the blank-prefixes edge', () => {
  assert.equal(isAllDefaultRow({ _id: 'x', name: 'a', ...defaultFields }), true);
  assert.equal(isAllDefaultRow({ _id: 'x', name: 'a', ...defaultFields, backend: 'b1' }), false);
  assert.equal(isAllDefaultRow({ _id: 'x', name: 'a', ...defaultFields, publicMode: 'entire' }), false);
  assert.equal(
    isAllDefaultRow({
      _id: 'x', name: 'a', ...defaultFields, publicMode: 'prefixes',
      public_prefixes: [{ id: 'p', value: '  ' }],
    }),
    true,
    'prefixes mode with only blanks overrides nothing',
  );
});

test('a brand-new all-default row (not in baseline) serialises NOTHING', () => {
  // a policy exists iff something is overridden.
  const res = buildBucketPayload([{ _id: 'x', name: 'fresh', ...defaultFields }], []);
  assert.equal(res.ok, true);
  if (!res.ok) return;
  assert.deepEqual(res.body.buckets, {}, 'all-default new row is a no-op');
});

test('an all-default row whose bucket IS in the baseline serialises name: null', () => {
  // reset-to-defaults deletes the policy.
  const res = buildBucketPayload([{ _id: 'x', name: 'was', ...defaultFields }], ['was']);
  assert.equal(res.ok, true);
  if (!res.ok) return;
  assert.deepEqual(res.body.buckets, { was: null });
});

test('a baseline bucket absent from the rows serialises name: null', () => {
  // removing a policy actually deletes it server-side.
  const res = buildBucketPayload(
    [{ _id: 'x', name: 'kept', ...defaultFields, backend: 'b1' }],
    ['kept', 'removed'],
  );
  assert.equal(res.ok, true);
  if (!res.ok) return;
  assert.deepEqual(Object.keys(res.body.buckets).sort(), ['kept', 'removed']);
  assert.equal(res.body.buckets.removed, null);
  assert.equal(res.body.buckets.kept?.backend, 'b1');
});

// ───────────────────────────────────────────────────────────────────
// LifecyclePanel — buildLifecyclePayload
// ───────────────────────────────────────────────────────────────────
function emptyDeleteRule(name: string): LifecycleRuleConfig {
  return {
    name,
    enabled: false,
    bucket: '',
    prefix: '',
    action: 'delete',
    expire_after: '30d',
    include_globs: [],
    exclude_globs: ['.deltaglider/**'],
    batch_size: 100,
  };
}

test('a delete rule normalises + trims; body matches the historical shape', () => {
  const cfg: LifecycleConfig = {
    ...DEFAULT_LIFECYCLE,
    enabled: true,
    rules: [
      {
        name: '  expire  ',
        enabled: true,
        bucket: '  prod  ',
        prefix: 'builds',
        action: 'delete',
        expire_after: '  30d ',
        include_globs: [],
        exclude_globs: ['.deltaglider/**'],
        batch_size: 0, // → 100
      },
    ],
  };
  const res = buildLifecyclePayload(cfg);
  assert.equal(res.ok, true);
  if (!res.ok) return;
  const rule = res.body.lifecycle?.rules[0];
  assert.equal(rule?.name, 'expire');
  assert.equal(rule?.bucket, 'prod');
  assert.equal(rule?.prefix, 'builds/'); // normalizePrefix adds trailing /
  assert.equal(rule?.expire_after, '30d');
  assert.equal(rule?.batch_size, 100);
  assert.equal(rule?.action, 'delete');
});

test('validation order: duplicate > missing-name > regex > bucket > expire > transition-destination', () => {
  // Spot-check the key gates.
  assert.equal(
    failureError(buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [
      { ...emptyDeleteRule('dup'), expire_after: '1d', bucket: 'b' },
      { ...emptyDeleteRule('dup'), expire_after: '1d', bucket: 'b' },
    ] })),
    'Duplicate rule name: dup',
  );
  assert.equal(
    failureError(buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [emptyDeleteRule('')] })),
    'Every lifecycle rule needs a name.',
  );
  assert.equal(
    failureError(buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{ ...emptyDeleteRule('bad name!'), bucket: 'b', expire_after: '1d' }] })),
    'Rule bad name!: names must match [A-Za-z0-9_.-]{1,64}.',
  );
  assert.equal(
    failureError(buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{ ...emptyDeleteRule('ok'), bucket: '', expire_after: '1d' }] })),
    'Rule ok: bucket is required.',
  );
  assert.equal(
    failureError(buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{ ...emptyDeleteRule('ok'), bucket: 'b', expire_after: '' }] })),
    'Rule ok: expire_after is required.',
  );
});

test('transition action requires a destination bucket', () => {
  const res = buildLifecyclePayload({
    ...DEFAULT_LIFECYCLE,
    rules: [
      {
        ...emptyDeleteRule('move'),
        bucket: 'src',
        expire_after: '1d',
        action: { type: 'transition', destination: { bucket: '', prefix: 'archive/' }, delete_source_after_success: false },
      },
    ],
  });
  assert.equal(res.ok, false);
  if (res.ok) return;
  assert.equal(res.error, 'Rule move: transition destination bucket is required.');
  assert.equal(actionKind({ type: 'transition', destination: { bucket: 'x' } }), 'transition');
  assert.equal(actionKind('delete'), 'delete');
});

test('retain-newest: actionKind, count validation, expire_after dropped, empty qualify fields stripped', () => {
  assert.equal(actionKind({ type: 'retain-newest', count: 2 }), 'retain-newest');

  // count < 1 is rejected (a 0 would empty the prefix — must be an explicit
  // delete rule, never a retain typo).
  assert.equal(
    failureError(buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{
      ...emptyDeleteRule('keep'), bucket: 'b',
      action: { type: 'retain-newest', count: 0 },
    }] })),
    'Rule keep: retain-newest count must be at least 1.',
  );

  // A valid retain rule needs NO expire_after, and any stray expire_after is
  // dropped from the wire; qualify with only min_size keeps just that field.
  const res = buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{
    ...emptyDeleteRule('keep-last-2'), bucket: 'db-archive', prefix: 'nightly',
    expire_after: '99d', // must be dropped
    action: {
      type: 'retain-newest',
      count: 2,
      qualify: { min_size_bytes: 1048576, min_age: '' /* empty → stripped */ },
      protect_younger_than: '',
    },
  }] });
  assert.equal(res.ok, true, JSON.stringify(res));
  if (!res.ok) return;
  const rule = res.body.lifecycle?.rules[0];
  assert.equal(rule?.expire_after, undefined, 'expire_after dropped for retain-newest');
  const action = rule?.action;
  assert.ok(action && typeof action === 'object' && action.type === 'retain-newest');
  if (!action || typeof action !== 'object' || action.type !== 'retain-newest') return;
  assert.equal(action.count, 2);
  assert.deepEqual(action.qualify, { min_size_bytes: 1048576 }, 'empty min_age stripped');
  assert.equal('protect_younger_than' in action, false, 'empty protect stripped');
});

test('normalizeLifecycle backfills defaults from emptyRule for partial rules, normalises a transition action', () => {
  const norm = normalizeLifecycle({ rules: [{ name: 'r', bucket: 'b' } as unknown as LifecycleRuleConfig] });
  const r = norm.rules[0];
  assert.equal(r.expire_after, '30d');
  assert.deepEqual(r.exclude_globs, ['.deltaglider/**']);
  assert.equal(r.batch_size, 100);
  assert.equal(r.action, 'delete'); // normalizeAction('delete') === 'delete'

  const normT = normalizeLifecycle({
    rules: [{
      name: 't', bucket: 'b',
      action: { type: 'transition', destination: { bucket: '  dst  ', prefix: 'arch' }, delete_source_after_success: true },
    } as unknown as LifecycleRuleConfig],
  });
  const rt = normT.rules[0].action;
  assert.ok(rt && typeof rt === 'object' && rt.type === 'transition');
  if (!rt || typeof rt !== 'object' || rt.type !== 'transition') return;
  assert.equal(rt.destination.bucket, 'dst'); // trimmed
  assert.equal(rt.destination.prefix, 'arch/'); // normalizePrefix
  assert.equal(rt.delete_source_after_success, true);
});

// ───────────────────────────────────────────────────────────────────
// ReplicationPanel — buildReplicationPayload
// ───────────────────────────────────────────────────────────────────
function emptyReplRule(name: string): ReplicationRuleConfig {
  return {
    name,
    enabled: true,
    source: { bucket: 'src', prefix: '' },
    destination: { bucket: 'dst', prefix: '' },
    interval: '15m',
    batch_size: 100,
    replicate_deletes: false,
    conflict: 'newer-wins',
    include_globs: [],
    exclude_globs: ['.dg/*'],
  };
}

test('a valid rule normalises source/destination prefixes; body matches', () => {
  const cfg: ReplicationConfig = {
    ...DEFAULT_REPLICATION,
    rules: [
      {
        name: 'mirror',
        enabled: true,
        source: { bucket: 'src', prefix: 'a' },
        destination: { bucket: 'dst', prefix: 'b' },
        interval: '15m',
        batch_size: 100,
        replicate_deletes: false,
        conflict: 'newer-wins',
        include_globs: [],
        exclude_globs: ['.dg/*'],
      },
    ],
  };
  const res = buildReplicationPayload(cfg);
  assert.equal(res.ok, true);
  if (!res.ok) return;
  const rule = res.body.replication?.rules[0];
  assert.equal(rule?.source.prefix, 'a/'); // normalizePrefix
  assert.equal(rule?.destination.prefix, 'b/');
  assert.equal(rule?.name, 'mirror');
});

test('validation: duplicate names, missing name, missing buckets', () => {
  assert.equal(
    failureError(buildReplicationPayload({ ...DEFAULT_REPLICATION, rules: [
      emptyReplRule('dup'), emptyReplRule('dup'),
    ] })),
    'Duplicate rule name: dup',
  );
  assert.equal(
    failureError(buildReplicationPayload({ ...DEFAULT_REPLICATION, rules: [emptyReplRule('  ')] })),
    'Every replication rule needs a name.',
  );
  assert.equal(
    failureError(buildReplicationPayload({ ...DEFAULT_REPLICATION, rules: [
      { ...emptyReplRule('ok'), source: { bucket: '', prefix: '' } },
    ] })),
    'Rule ok: source and destination buckets are required.',
  );
});

test('normalizeReplication backfills defaults + nested source/destination', () => {
  const norm = normalizeReplication({ rules: [{ name: 'r' } as unknown as ReplicationRuleConfig] });
  const r = norm.rules[0];
  assert.deepEqual(r.source, { bucket: '', prefix: '' });
  assert.deepEqual(r.destination, { bucket: '', prefix: '' });
  assert.deepEqual(r.exclude_globs, ['.dg/*']);
  assert.equal(r.conflict, 'newer-wins');
});
