import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile a TS module to an importable data: URL. The three payload
// modules are pure (no React / antd) so they import directly. They DO
// import `../storagePath` (also pure), so we resolve that dependency by
// transpiling it too and rewriting the import specifier to the data URL.
async function transpile(relPath, fileName) {
  const url = new URL(relPath, import.meta.url);
  const source = await readFile(url, 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ES2020,
      target: ts.ScriptTarget.ES2020,
    },
    fileName,
  });
  return outputText;
}

function dataUrl(source) {
  return `data:text/javascript;base64,${Buffer.from(source).toString('base64')}`;
}

// Pure runtime deps the payload modules pull in: `../storagePath` (all three)
// and `./ruleNames` (lifecycle + replication, for next-unique-rule-name). Both
// are React-free, so transpile them to data URLs and rewrite the specifiers.
const storagePathUrl = dataUrl(await transpile('../src/storagePath.ts', 'storagePath.ts'));
const ruleNamesUrl = dataUrl(await transpile('../src/components/ruleNames.ts', 'ruleNames.ts'));

async function loadPayloadModule(relPath, fileName) {
  let out = await transpile(relPath, fileName);
  // Rewrite bare relative specifiers to their transpiled data URLs.
  out = out.replace(/(['"])\.\.\/storagePath\1/g, JSON.stringify(storagePathUrl));
  out = out.replace(/(['"])\.\/ruleNames\1/g, JSON.stringify(ruleNamesUrl));
  return import(dataUrl(out));
}

const bucket = await loadPayloadModule(
  '../src/components/bucketPolicyPayload.ts',
  'bucketPolicyPayload.ts'
);
const lifecycle = await loadPayloadModule(
  '../src/components/lifecyclePayload.ts',
  'lifecyclePayload.ts'
);
const replication = await loadPayloadModule(
  '../src/components/replicationPayload.ts',
  'replicationPayload.ts'
);

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
const { buildBucketPayload, freshId, policyToRow, isAllDefaultRow } = bucket;

const defaultFields = {
  compression: null,
  max_delta_ratio: null,
  backend: '',
  alias: '',
  publicMode: 'none',
  public_prefixes: [],
  quota_bytes: null,
};

// (1) Synthetic ids are unique + monotonic, and NEVER appear in the wire.
{
  const a = freshId();
  const b = freshId();
  assert.notEqual(a, b, 'freshId must be collision-free');
  assert.ok(a.startsWith('bkt-') && b.startsWith('bkt-'));
}

// (2) Empty-name rows are dropped; a populated row serialises every
//     clearable field explicitly (value or null) and no `_id`.
{
  const rows = [
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
    },
  ];
  const res = buildBucketPayload(rows);
  assert.equal(res.ok, true);
  assert.deepEqual(Object.keys(res.body.buckets), ['prod'], 'unnamed row dropped');
  const p = res.body.buckets.prod;
  assert.ok(!('_id' in p), 'synthetic id must never reach the wire');
  assert.deepEqual(p, {
    compression: false,
    max_delta_ratio: 0.5,
    backend: 'b1',
    alias: 'realprod',
    public: null,
    public_prefixes: null,
    quota_bytes: 1073741824,
  });
}

// (3) compression:null is preserved as explicit null (merge-clears the key).
{
  const res = buildBucketPayload([
    { _id: 'bkt-1', name: 'b', ...defaultFields, backend: 'b1' },
  ]);
  assert.equal(res.ok, true);
  assert.equal(res.body.buckets.b.compression, null);
  const json = JSON.stringify(res.body);
  assert.ok(json.includes('"compression":null'), 'null compression must survive stringify');
}

// (4) Tri-state public mode → wire sentinels. `entire` => [""]; `prefixes`
//     drops blanks; `none` emits EXPLICIT nulls so the merge clears any
//     previous public state (omission would keep the bucket public).
{
  const entire = buildBucketPayload([
    { _id: 'bkt-e', name: 'e', ...defaultFields, publicMode: 'entire' },
  ]);
  assert.deepEqual(entire.body.buckets.e.public_prefixes, ['']);
  assert.equal(entire.body.buckets.e.public, null, 'shorthand `public` always cleared; [""] is the wire spelling');

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
  assert.deepEqual(prefixes.body.buckets.p.public_prefixes, ['builds/', 'rel/']);

  const none = buildBucketPayload([
    { _id: 'bkt-n', name: 'n', ...defaultFields, backend: 'b1' },
  ]);
  assert.equal(none.body.buckets.n.public_prefixes, null, 'none emits explicit null (merge-clears)');
  assert.equal(none.body.buckets.n.public, null);
}

// (5) Duplicate bucket names abort with an error, zero body.
{
  const res = buildBucketPayload([
    { _id: '1', name: 'dup', ...defaultFields },
    { _id: '2', name: 'dup', ...defaultFields },
  ]);
  assert.equal(res.ok, false);
  assert.equal(res.error, 'Duplicate bucket name: dup');
}

// (6) policyToRow round-trip through buildBucketPayload: public:true
//     shorthand decodes to `entire` and re-encodes to [""].
{
  const row = policyToRow('shorthand', { public: true });
  assert.equal(row.publicMode, 'entire');
  const res = buildBucketPayload([row]);
  assert.deepEqual(res.body.buckets.shorthand.public_prefixes, ['']);
  const row2 = policyToRow('expanded', { public_prefixes: [''] });
  assert.equal(row2.publicMode, 'entire');
  assert.deepEqual(buildBucketPayload([row2]).body.buckets.expanded.public_prefixes, ['']);
}

// (7) isAllDefaultRow truth table — incl. the blank-prefixes edge.
{
  assert.equal(isAllDefaultRow({ _id: 'x', name: 'a', ...defaultFields }), true);
  assert.equal(isAllDefaultRow({ _id: 'x', name: 'a', ...defaultFields, backend: 'b1' }), false);
  assert.equal(isAllDefaultRow({ _id: 'x', name: 'a', ...defaultFields, publicMode: 'entire' }), false);
  assert.equal(
    isAllDefaultRow({
      _id: 'x', name: 'a', ...defaultFields, publicMode: 'prefixes',
      public_prefixes: [{ id: 'p', value: '  ' }],
    }),
    true,
    'prefixes mode with only blanks overrides nothing'
  );
}

// (8) A brand-new all-default row (not in baseline) serialises NOTHING —
//     a policy exists iff something is overridden.
{
  const res = buildBucketPayload([{ _id: 'x', name: 'fresh', ...defaultFields }], []);
  assert.deepEqual(res.body.buckets, {}, 'all-default new row is a no-op');
}

// (9) An all-default row whose bucket IS in the baseline serialises
//     `name: null` — reset-to-defaults deletes the policy.
{
  const res = buildBucketPayload([{ _id: 'x', name: 'was', ...defaultFields }], ['was']);
  assert.deepEqual(res.body.buckets, { was: null });
}

// (10) A baseline bucket absent from the rows serialises `name: null` —
//      removing a policy actually deletes it server-side.
{
  const res = buildBucketPayload(
    [{ _id: 'x', name: 'kept', ...defaultFields, backend: 'b1' }],
    ['kept', 'removed']
  );
  assert.deepEqual(Object.keys(res.body.buckets).sort(), ['kept', 'removed']);
  assert.equal(res.body.buckets.removed, null);
  assert.equal(res.body.buckets.kept.backend, 'b1');
}

// ───────────────────────────────────────────────────────────────────
// LifecyclePanel — buildLifecyclePayload
// ───────────────────────────────────────────────────────────────────
const {
  buildLifecyclePayload,
  DEFAULT_LIFECYCLE,
  normalizeLifecycle,
  actionKind,
} = lifecycle;

// (7) A delete rule normalises + trims; body matches the historical shape.
{
  const cfg = {
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
  const rule = res.body.lifecycle.rules[0];
  assert.equal(rule.name, 'expire');
  assert.equal(rule.bucket, 'prod');
  assert.equal(rule.prefix, 'builds/'); // normalizePrefix adds trailing /
  assert.equal(rule.expire_after, '30d');
  assert.equal(rule.batch_size, 100);
  assert.equal(rule.action, 'delete');
}

// (8) Validation order: duplicate > missing-name > regex > bucket > expire
//     > transition-destination. Spot-check the key gates.
{
  assert.equal(
    buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [
      { ...emptyDeleteRule('dup'), expire_after: '1d', bucket: 'b' },
      { ...emptyDeleteRule('dup'), expire_after: '1d', bucket: 'b' },
    ] }).error,
    'Duplicate rule name: dup'
  );
  assert.equal(
    buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [emptyDeleteRule('')] }).error,
    'Every lifecycle rule needs a name.'
  );
  assert.equal(
    buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{ ...emptyDeleteRule('bad name!'), bucket: 'b', expire_after: '1d' }] }).error,
    'Rule bad name!: names must match [A-Za-z0-9_.-]{1,64}.'
  );
  assert.equal(
    buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{ ...emptyDeleteRule('ok'), bucket: '', expire_after: '1d' }] }).error,
    'Rule ok: bucket is required.'
  );
  assert.equal(
    buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{ ...emptyDeleteRule('ok'), bucket: 'b', expire_after: '' }] }).error,
    'Rule ok: expire_after is required.'
  );
}

// (9) Transition action requires a destination bucket.
{
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
  assert.equal(res.error, 'Rule move: transition destination bucket is required.');
  assert.equal(actionKind({ type: 'transition', destination: { bucket: 'x' } }), 'transition');
  assert.equal(actionKind('delete'), 'delete');
}

// (9b) retain-newest: actionKind, count validation, expire_after dropped,
//      empty qualify fields stripped from the wire.
{
  assert.equal(
    actionKind({ type: 'retain-newest', count: 2 }),
    'retain-newest'
  );

  // count < 1 is rejected (a 0 would empty the prefix — must be an explicit
  // delete rule, never a retain typo).
  assert.equal(
    buildLifecyclePayload({ ...DEFAULT_LIFECYCLE, rules: [{
      ...emptyDeleteRule('keep'), bucket: 'b',
      action: { type: 'retain-newest', count: 0 },
    }] }).error,
    'Rule keep: retain-newest count must be at least 1.'
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
  const rule = res.body.lifecycle.rules[0];
  assert.equal(rule.expire_after, undefined, 'expire_after dropped for retain-newest');
  assert.equal(rule.action.type, 'retain-newest');
  assert.equal(rule.action.count, 2);
  assert.deepEqual(rule.action.qualify, { min_size_bytes: 1048576 }, 'empty min_age stripped');
  assert.equal('protect_younger_than' in rule.action, false, 'empty protect stripped');
}

// (10) normalizeLifecycle backfills defaults from emptyRule for partial rules,
//      and normalises a transition action (exercises normalizeAction).
{
  const norm = normalizeLifecycle({ rules: [{ name: 'r', bucket: 'b' }] });
  const r = norm.rules[0];
  assert.equal(r.expire_after, '30d');
  assert.deepEqual(r.exclude_globs, ['.deltaglider/**']);
  assert.equal(r.batch_size, 100);
  assert.equal(r.action, 'delete'); // normalizeAction('delete') === 'delete'

  const normT = normalizeLifecycle({
    rules: [{
      name: 't', bucket: 'b',
      action: { type: 'transition', destination: { bucket: '  dst  ', prefix: 'arch' }, delete_source_after_success: true },
    }],
  });
  const rt = normT.rules[0].action;
  assert.equal(rt.type, 'transition');
  assert.equal(rt.destination.bucket, 'dst'); // trimmed
  assert.equal(rt.destination.prefix, 'arch/'); // normalizePrefix
  assert.equal(rt.delete_source_after_success, true);
}

function emptyDeleteRule(name) {
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

// ───────────────────────────────────────────────────────────────────
// ReplicationPanel — buildReplicationPayload
// ───────────────────────────────────────────────────────────────────
const {
  buildReplicationPayload,
  DEFAULT_REPLICATION,
  normalizeReplication,
} = replication;

// (11) A valid rule normalises source/destination prefixes; body matches.
{
  const cfg = {
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
  const rule = res.body.replication.rules[0];
  assert.equal(rule.source.prefix, 'a/'); // normalizePrefix
  assert.equal(rule.destination.prefix, 'b/');
  assert.equal(rule.name, 'mirror');
}

// (12) Validation: duplicate names, missing name, missing buckets.
{
  assert.equal(
    buildReplicationPayload({ ...DEFAULT_REPLICATION, rules: [
      emptyReplRule('dup'), emptyReplRule('dup'),
    ] }).error,
    'Duplicate rule name: dup'
  );
  assert.equal(
    buildReplicationPayload({ ...DEFAULT_REPLICATION, rules: [emptyReplRule('  ')] }).error,
    'Every replication rule needs a name.'
  );
  assert.equal(
    buildReplicationPayload({ ...DEFAULT_REPLICATION, rules: [
      { ...emptyReplRule('ok'), source: { bucket: '', prefix: '' } },
    ] }).error,
    'Rule ok: source and destination buckets are required.'
  );
}

// (13) normalizeReplication backfills defaults + nested source/destination.
{
  const norm = normalizeReplication({ rules: [{ name: 'r' }] });
  const r = norm.rules[0];
  assert.deepEqual(r.source, { bucket: '', prefix: '' });
  assert.deepEqual(r.destination, { bucket: '', prefix: '' });
  assert.deepEqual(r.exclude_globs, ['.dg/*']);
  assert.equal(r.conflict, 'newer-wins');
}

function emptyReplRule(name) {
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

console.log('storage section payload regression checks passed');
