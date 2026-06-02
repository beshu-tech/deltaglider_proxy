import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile the pure payload helper (no React/antd) to an importable data: URL.
async function loadModule(relPath, fileName) {
  const url = new URL(relPath, import.meta.url);
  const source = await readFile(url, 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ES2020,
      target: ts.ScriptTarget.ES2020,
      importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
    },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const url = await loadModule('../src/components/webhookDeliveryPayload.ts', 'webhookDeliveryPayload.ts');
const { formFromWire, buildPayloadFromForm, resolveSlackChannelsPreview } = await import(url);

// Must equal REDACTED_SENTINEL in src/config.rs and the constant in
// webhookDeliveryPayload.ts — the cross-language secret-mask sentinel.
const SENTINEL = '__redacted__';

// Deterministic id generator (the panel injects a per-instance counter).
function mkIdGen() {
  let n = 0;
  return () => `r${n++}`;
}

// Load a form from a server wire body, optionally mutate it, then build the
// payload — exactly the panel's pick → edit → toPayload pipeline.
function buildFromWire(wire, mutate) {
  const form = formFromWire(wire, mkIdGen());
  if (mutate) mutate(form);
  return buildPayloadFromForm(form);
}
function headers(res) {
  return res.body.event_delivery.webhook_headers;
}

// ── legacy webhook_url folds into the endpoint list (deduped, legacy first) ──
{
  const form = formFromWire(
    { webhook_url: 'https://a.example/in', webhook_urls: ['https://b.example/in'] },
    mkIdGen()
  );
  assert.deepEqual(
    form.urlRows.map((r) => r.url),
    ['https://a.example/in', 'https://b.example/in'],
    'legacy url first, deduped'
  );
}
{
  const form = formFromWire(
    { webhook_url: 'https://x.example/h', webhook_urls: ['https://x.example/h'] },
    mkIdGen()
  );
  assert.deepEqual(form.urlRows.map((r) => r.url), ['https://x.example/h'], 'dedupe legacy vs list');
}

// ── legacy webhook_url is always cleared on save; list preserved ──
{
  const res = buildFromWire({ webhook_url: 'https://legacy.example/h' });
  assert.ok(res.ok);
  assert.equal(res.body.event_delivery.webhook_url, null, 'legacy webhook_url cleared');
  assert.deepEqual(res.body.event_delivery.webhook_urls, ['https://legacy.example/h']);
}

// ── header DELETE emits explicit null (RFC 7396) for a REMOVED row ──
{
  const res = buildFromWire(
    { webhook_headers: { Authorization: SENTINEL, 'X-Env': SENTINEL } },
    (f) => {
      f.headerRows = f.headerRows.filter((r) => r.name !== 'X-Env'); // operator removed it
    }
  );
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.equal(headers(res)['X-Env'], null, 'removed header → null delete');
  assert.equal(headers(res)['Authorization'], SENTINEL, 'untouched header stays sentinel');
  const rt = JSON.parse(JSON.stringify(res.body));
  assert.ok('X-Env' in rt.event_delivery.webhook_headers, 'null delete-key survives JSON.stringify');
}

// ── untouched masked secret passes through as sentinel (server restores) ──
{
  const res = buildFromWire({ webhook_headers: { Authorization: SENTINEL } });
  assert.ok(res.ok);
  assert.equal(headers(res)['Authorization'], SENTINEL);
}

// ── retyped secret + new header pass through with real values ──
{
  const res = buildFromWire({ webhook_headers: { Authorization: SENTINEL } }, (f) => {
    f.headerRows[0].value = 'Bearer NEW';
    f.headerRows[0].masked = false; // typing unmasks
    f.headerRows.push({ id: 'x', name: 'X-Trace', value: 'on', origName: '', masked: false });
  });
  assert.ok(res.ok);
  assert.equal(headers(res)['Authorization'], 'Bearer NEW');
  assert.equal(headers(res)['X-Trace'], 'on');
}

// ── RENAME a still-masked header is BLOCKED (adversarial #2) ──
{
  const res = buildFromWire({ webhook_headers: { Authorizaton: SENTINEL } }, (f) => {
    f.headerRows[0].name = 'Authorization'; // fix typo without re-entering value
  });
  assert.ok(!res.ok, 'rename-while-masked must be blocked');
  assert.ok(
    res.errors.some((e) => /re-enter/i.test(e) || /re-type/i.test(e)),
    'error must tell operator to re-type the value'
  );
}

// ── rename WITH a re-typed value is allowed; old name deleted ──
{
  const res = buildFromWire({ webhook_headers: { Authorizaton: SENTINEL } }, (f) => {
    f.headerRows[0].name = 'Authorization';
    f.headerRows[0].value = 'Bearer real';
    f.headerRows[0].masked = false;
  });
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.equal(headers(res)['Authorization'], 'Bearer real');
  assert.equal(headers(res)['Authorizaton'], null, 'old name deleted');
}

// ── in-progress empty rows are ignored, not errors ──
{
  const res = buildFromWire({}, (f) => {
    f.urlRows = [{ id: 'a', url: '' }];
    f.headerRows = [{ id: 'b', name: '', value: '', origName: '', masked: false }];
  });
  assert.ok(res.ok, `empty in-progress rows must not error, got ${JSON.stringify(res.errors)}`);
  assert.deepEqual(res.body.event_delivery.webhook_urls, []);
}

// ── enabling with zero endpoints is rejected (usability trap) ──
{
  const res = buildFromWire({}, (f) => {
    f.enabled = true;
    f.urlRows = [];
  });
  assert.ok(!res.ok && res.errors.some((e) => /no endpoint/i.test(e)), 'enabled + no endpoint must fail');
}

// ── duration validation: simple + compound humantime (1h30m) + word forms ──
{
  assert.ok(buildFromWire({}, (f) => (f.tick_interval = '30s')).ok, '30s ok');
  assert.ok(buildFromWire({}, (f) => (f.delivered_retention = '0s')).ok, '0s ok (disable prune)');
  assert.ok(buildFromWire({}, (f) => (f.retry_max = '1h30m')).ok, 'compound 1h30m ok (adversarial #4)');
  assert.ok(buildFromWire({}, (f) => (f.tick_interval = '1500ms')).ok, '1500ms ok');
  assert.ok(buildFromWire({}, (f) => (f.delivered_retention = '7days')).ok, '7days word-form ok');
  const bad = buildFromWire({}, (f) => (f.tick_interval = '30'));
  assert.ok(!bad.ok && bad.errors.some((e) => /tick_interval/.test(e)), 'bare number rejected');
  assert.ok(!buildFromWire({}, (f) => (f.request_timeout = '5 minutes')).ok, 'prose duration rejected');
}

// ── numeric range validation ──
{
  const bad = buildFromWire({}, (f) => (f.batch_size = 0));
  assert.ok(!bad.ok && bad.errors.some((e) => /batch_size/.test(e)), 'batch_size 0 rejected (min 1)');
  assert.ok(!buildFromWire({}, (f) => (f.max_attempts = -1)).ok, 'negative max_attempts rejected');
}

// ── invalid endpoint URL + invalid header name rejected ──
{
  const u = buildFromWire({}, (f) => {
    f.enabled = true;
    f.urlRows = [{ id: 'a', url: 'not-a-url' }];
  });
  assert.ok(!u.ok && u.errors.some((e) => /not a valid/i.test(e)), 'junk URL rejected');
  const h = buildFromWire({}, (f) => {
    f.headerRows = [{ id: 'b', name: 'Bad Header', value: 'v', origName: '', masked: false }];
  });
  assert.ok(!h.ok && h.errors.some((e) => /invalid characters/i.test(e)), 'header name with space rejected');
}

// ─────────────────────────────────────────────────────────────────────────
// Slack format
// ─────────────────────────────────────────────────────────────────────────
function ed(res) {
  return res.body.event_delivery;
}

// ── format: raw still round-trips unchanged (no slack fields leak as set) ──
{
  const res = buildFromWire({ webhook_urls: ['https://hooks.example/in'] });
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.equal(ed(res).format, 'raw', 'format defaults to raw');
  assert.equal(ed(res).slack_bot_token, null, 'no token in raw mode → null');
  assert.deepEqual(ed(res).webhook_urls, ['https://hooks.example/in']);
}

// ── slack bot token: untouched (masked) → passes through as the sentinel ──
{
  const res = buildFromWire({
    format: 'slack',
    slack_bot_token: SENTINEL,
    slack_channel: '#deploys',
  });
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.equal(ed(res).format, 'slack');
  assert.equal(ed(res).slack_bot_token, SENTINEL, 'untouched token → sentinel (server restores)');
  assert.equal(ed(res).slack_channel, '#deploys');
}

// ── slack bot token: retyped → real value flows through ──
{
  const res = buildFromWire(
    { format: 'slack', slack_bot_token: SENTINEL, slack_channel: '#deploys' },
    (f) => {
      f.slackBotToken = 'xoxb-NEW-TOKEN';
      f.slackBotTokenMasked = false; // typing unmasks
    }
  );
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.equal(ed(res).slack_bot_token, 'xoxb-NEW-TOKEN', 'retyped token → real value');
}

// ── slack bot-token mode needs a channel ──
{
  const res = buildFromWire({
    format: 'slack',
    slack_bot_token: 'xoxb-real',
    // no channel
  });
  assert.ok(!res.ok, 'bot token without channel must fail');
  assert.ok(
    res.errors.some((e) => /needs a channel/i.test(e)),
    'error must mention the missing channel'
  );
}

// ── a masked (untouched) token also counts as bot mode → still needs channel ──
{
  const res = buildFromWire({ format: 'slack', slack_bot_token: SENTINEL });
  assert.ok(!res.ok && res.errors.some((e) => /needs a channel/i.test(e)), 'masked token = bot mode needs channel');
}

// ── slack webhook mode (no token), enabled, no URL → must fail ──
{
  const res = buildFromWire({ format: 'slack' }, (f) => {
    f.enabled = true;
    f.urlRows = [];
  });
  assert.ok(!res.ok, 'enabled webhook-mode Slack with no URL must fail');
  assert.ok(
    res.errors.some((e) => /webhook url/i.test(e) || /no slack incoming webhook/i.test(e)),
    'error must mention the missing Slack webhook URL'
  );
}

// ── slack webhook mode with a hooks.slack.com URL → ok ──
{
  const res = buildFromWire(
    { format: 'slack', webhook_urls: ['https://hooks.slack.com/services/T/B/x'] },
    (f) => {
      f.enabled = true;
    }
  );
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.equal(ed(res).format, 'slack');
  assert.equal(ed(res).slack_bot_token, null, 'webhook mode → no bot token');
  assert.deepEqual(ed(res).webhook_urls, ['https://hooks.slack.com/services/T/B/x']);
}

// ── slack notify kinds: at least one must be selected ──
{
  const res = buildFromWire(
    { format: 'slack', webhook_urls: ['https://hooks.slack.com/services/T/B/x'] },
    (f) => {
      f.slackNotifyKinds = [];
    }
  );
  assert.ok(!res.ok && res.errors.some((e) => /at least one event kind/i.test(e)), 'no notify kinds must fail');
}

// ── slack notify kinds + globs round-trip; empty glob row rejected ──
{
  const ok = buildFromWire({
    format: 'slack',
    webhook_urls: ['https://hooks.slack.com/services/T/B/x'],
    slack_notify_kinds: ['ObjectCreated', 'ObjectDeleted'],
    slack_include_globs: ['releases/**'],
    slack_exclude_globs: ['tmp/**'],
  });
  assert.ok(ok.ok, JSON.stringify(ok.errors));
  assert.deepEqual(ed(ok).slack_notify_kinds, ['ObjectCreated', 'ObjectDeleted']);
  assert.deepEqual(ed(ok).slack_include_globs, ['releases/**']);
  assert.deepEqual(ed(ok).slack_exclude_globs, ['tmp/**']);

  const bad = buildFromWire(
    { format: 'slack', webhook_urls: ['https://hooks.slack.com/services/T/B/x'] },
    (f) => {
      f.slackIncludeRows = [{ id: 'g1', glob: '  ' }]; // blank → error
    }
  );
  assert.ok(!bad.ok && bad.errors.some((e) => /prefix filter is empty/i.test(e)), 'blank glob row rejected');
}

// ── slack cosmetic fields (webhook mode) flow through; empty → null ──
{
  const res = buildFromWire(
    { format: 'slack', webhook_urls: ['https://hooks.slack.com/services/T/B/x'] },
    (f) => {
      f.slackUsername = 'DeltaGlider';
      f.slackIconEmoji = ':package:';
    }
  );
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.equal(ed(res).slack_username, 'DeltaGlider');
  assert.equal(ed(res).slack_icon_emoji, ':package:');
  const empty = buildFromWire({ format: 'slack', webhook_urls: ['https://hooks.slack.com/services/T/B/x'] });
  assert.equal(ed(empty).slack_username, null, 'empty username → null (merge-patch clears)');
}

// ─────────────────────────────────────────────────────────────────────────
// Slack channel routing (per bucket / prefix → channel)
// ─────────────────────────────────────────────────────────────────────────

// ── route round-trip: wire → form → payload (bot-token mode) ──
{
  const res = buildFromWire({
    format: 'slack',
    slack_bot_token: SENTINEL,
    slack_channel: '#default',
    slack_routes: [
      { name: 'CI', bucket: 'releases', prefix_globs: ['builds/**'], channel: '#ci' },
      { bucket: 'audit', channel: '#audit' },
    ],
  });
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.deepEqual(
    ed(res).slack_routes,
    [
      { channel: '#ci', name: 'CI', bucket: 'releases', prefix_globs: ['builds/**'] },
      { channel: '#audit', bucket: 'audit' },
    ],
    'routes round-trip; empty name/globs omitted from the wire'
  );
  assert.equal(ed(res).slack_channel, '#default', 'fallback channel coexists with routes');
}

// ── empty-channel route is dropped (in-progress / blank) ──
{
  const res = buildFromWire(
    { format: 'slack', slack_bot_token: SENTINEL, slack_channel: '#default' },
    (f) => {
      f.slackRoutes = [
        { id: 'rt0', name: '', bucket: '', prefixGlobs: [], channel: '' }, // fully blank → dropped
        { id: 'rt1', name: '', bucket: 'b', prefixGlobs: [], channel: '#real' },
      ];
    }
  );
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.deepEqual(ed(res).slack_routes, [{ channel: '#real', bucket: 'b' }], 'blank route dropped, real route kept');
}

// ── a half-filled route (bucket but no channel) is an error, not silently dropped ──
{
  const res = buildFromWire(
    { format: 'slack', slack_bot_token: SENTINEL, slack_channel: '#default' },
    (f) => {
      f.slackRoutes = [{ id: 'rt0', name: 'oops', bucket: 'b', prefixGlobs: [], channel: '' }];
    }
  );
  assert.ok(!res.ok && res.errors.some((e) => /Route 1 needs a channel/.test(e)), 'half-filled route needs a channel');
}

// ── prefix_globs round-trip (trimmed, blanks dropped) ──
{
  const res = buildFromWire(
    { format: 'slack', slack_bot_token: SENTINEL, slack_channel: '#default' },
    (f) => {
      f.slackRoutes = [
        {
          id: 'rt0',
          name: '',
          bucket: '',
          prefixGlobs: [
            { id: 'g0', glob: ' builds/** ' },
            { id: 'g1', glob: '  ' }, // blank → dropped
            { id: 'g2', glob: 'dist/*' },
          ],
          channel: '#ci',
        },
      ];
    }
  );
  assert.ok(res.ok, JSON.stringify(res.errors));
  assert.deepEqual(ed(res).slack_routes, [{ channel: '#ci', prefix_globs: ['builds/**', 'dist/*'] }], 'globs trimmed, blanks dropped');
}

// ── routes present but NOT bot-token mode → surfaced error ──
{
  const res = buildFromWire(
    { format: 'slack', webhook_urls: ['https://hooks.slack.com/services/T/B/x'] },
    (f) => {
      f.enabled = true;
      f.slackRoutes = [{ id: 'rt0', name: '', bucket: 'b', prefixGlobs: [], channel: '#ci' }];
    }
  );
  assert.ok(!res.ok && res.errors.some((e) => /needs a bot token/i.test(e)), 'routing without bot token must error');
}

// ── routes + empty fallback channel coexist (allowed) ──
{
  const res = buildFromWire(
    { format: 'slack', slack_bot_token: SENTINEL },
    (f) => {
      f.slackChannel = ''; // no fallback
      f.slackRoutes = [{ id: 'rt0', name: '', bucket: '', prefixGlobs: [], channel: '#ci' }];
    }
  );
  assert.ok(res.ok, `routes + empty fallback channel must be allowed, got ${JSON.stringify(res.errors)}`);
  assert.equal(ed(res).slack_channel, null, 'empty fallback channel → null');
  assert.deepEqual(ed(res).slack_routes, [{ channel: '#ci' }]);
}

// ─────────────────────────────────────────────────────────────────────────
// resolveSlackChannelsPreview — mirrors src/slack_format.rs::resolve_channels
// ─────────────────────────────────────────────────────────────────────────
function route(name, bucket, globs, channel) {
  return {
    id: name + channel,
    name: name ?? '',
    bucket: bucket ?? '',
    prefixGlobs: (globs ?? []).map((g, i) => ({ id: `g${i}`, glob: g })),
    channel,
  };
}

// ── no routes → fall back to the single channel ──
{
  const r = resolveSlackChannelsPreview([], '#default', 'b', 'k.zip');
  assert.deepEqual(r.matches, []);
  assert.ok(r.fellBackToChannel && r.fallbackChannel === '#default', 'falls back to single channel');
}

// ── bucket-scoped route matches its bucket, misses others ──
{
  const routes = [route('CI', 'releases', [], '#ci')];
  const hit = resolveSlackChannelsPreview(routes, '#default', 'releases', 'x.zip');
  assert.deepEqual(hit.matches.map((m) => m.channel), ['#ci'], 'bucket route matches');
  // Routes EXIST but none match → backend posts NOWHERE (no fallback to the
  // single channel once routes are configured). Preview must NOT claim fallback.
  const miss = resolveSlackChannelsPreview(routes, '#default', 'scratch', 'x.zip');
  assert.ok(
    miss.matches.length === 0 && !miss.fellBackToChannel,
    'non-matching bucket with routes present → no channel (NOT fallback)'
  );
}

// ── fan-out: an event matches multiple routes → all channels (deduped) ──
{
  const routes = [
    route('any', '', [], '#all'),
    route('rel', 'releases', [], '#rel'),
    route('dup', 'releases', [], '#all'), // same channel as first → deduped
  ];
  const r = resolveSlackChannelsPreview(routes, '#default', 'releases', 'x.zip');
  assert.deepEqual(r.matches.map((m) => m.channel), ['#all', '#rel'], 'fan-out, deduped, order-preserving');
  assert.ok(!r.fellBackToChannel, 'matched → no fallback');
}

// ── prefix-glob match (** crosses /, * within segment) ──
{
  const routes = [route('builds', '', ['builds/**'], '#ci')];
  assert.deepEqual(
    resolveSlackChannelsPreview(routes, '#default', 'any', 'builds/sub/app.zip').matches.map((m) => m.channel),
    ['#ci'],
    '** crosses slashes'
  );
  assert.ok(
    resolveSlackChannelsPreview(routes, '#default', 'any', 'dist/app.zip').matches.length === 0,
    'non-matching prefix → no channel'
  );
  const seg = [route('seg', '', ['dist/*.zip'], '#seg')];
  assert.deepEqual(
    resolveSlackChannelsPreview(seg, '#default', 'any', 'dist/app.zip').matches.map((m) => m.channel),
    ['#seg'],
    '* matches within a segment'
  );
  assert.ok(
    resolveSlackChannelsPreview(seg, '#default', 'any', 'dist/sub/app.zip').matches.length === 0,
    '* does not cross a slash'
  );
}

// ── no match + no fallback → no channel ──
{
  const routes = [route('rel', 'releases', [], '#rel')];
  const r = resolveSlackChannelsPreview(routes, '', 'scratch', 'x');
  assert.ok(r.matches.length === 0 && !r.fellBackToChannel, 'no match, no fallback → posted nowhere');
}

console.log('webhook-delivery-payload-regression-test: all assertions passed');
