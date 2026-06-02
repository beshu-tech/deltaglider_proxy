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
const { formFromWire, buildPayloadFromForm } = await import(url);

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

console.log('webhook-delivery-payload-regression-test: all assertions passed');
