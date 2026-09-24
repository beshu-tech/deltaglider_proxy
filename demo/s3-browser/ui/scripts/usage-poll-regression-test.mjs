import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Regression guard for COMPUTE-SIZE-POLL-FOREVER: the folder-size poll was a
// setInterval(async) that swallowed every error, so an expired session or a
// 403 polled every 2s forever (and slow responses overlapped). usagePollStep
// decides after each poll: done / retry / fail — and retries are bounded.

async function load(file, rewrite = {}) {
  const source = await readFile(new URL(`../src/${file}`, import.meta.url), 'utf8');
  let { outputText } = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
    fileName: file,
  });
  for (const [from, to] of Object.entries(rewrite)) {
    outputText = outputText.replaceAll(`from '${from}'`, `from '${to}'`);
  }
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}
const errUrl = await load('errorHandling.ts');
const { ApiError } = await import(errUrl);
const { usagePollStep } = await import(
  await load('usagePoll.ts', { './errorHandling': errUrl }),
);
// The budget is module-private; find it by probing, and pin its range.
let USAGE_POLL_MAX_ATTEMPTS = 1;
while (usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { result: null }).kind === 'retry') {
  USAGE_POLL_MAX_ATTEMPTS += 1;
  assert.ok(USAGE_POLL_MAX_ATTEMPTS < 10_000, 'the poll must be bounded');
}
assert.ok(USAGE_POLL_MAX_ATTEMPTS >= 30, 'budget covers at least ~1 minute at 2s');

const kind = (s) => s.kind;

// result → done; not-yet-cached → retry
assert.equal(kind(usagePollStep(1, { result: { total_size: 1 } })), 'done');
assert.equal(kind(usagePollStep(1, { result: null })), 'retry');

// transient errors retry
assert.equal(kind(usagePollStep(1, { error: new TypeError('Failed to fetch') })), 'retry');
assert.equal(kind(usagePollStep(1, { error: new ApiError('boom', 502) })), 'retry');
assert.equal(kind(usagePollStep(1, { error: new ApiError('slow', 429) })), 'retry');
assert.equal(kind(usagePollStep(1, { error: new ApiError('slow', 408) })), 'retry');

// non-retryable errors stop at once
const expired = usagePollStep(1, { error: new ApiError('Unauthorized', 401) });
assert.deepEqual(expired, { kind: 'fail', error: 'Session expired — sign in again' });
assert.equal(
  kind(usagePollStep(1, { error: new ApiError('Forbidden', 403, undefined, 'admin_session_required') })),
  'fail',
);
assert.deepEqual(usagePollStep(1, { error: new ApiError('Bad bucket', 400) }), { kind: 'fail', error: 'Bad bucket' });

// the attempt budget bounds both "no result yet" and transient errors
assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS - 1, { result: null })), 'retry');
assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { result: null })), 'fail');
assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { error: new ApiError('boom', 503) })), 'fail');
// ...but a result on the last attempt still wins
assert.equal(kind(usagePollStep(USAGE_POLL_MAX_ATTEMPTS, { result: { total_size: 1 } })), 'done');

console.log('usage-poll regression checks passed');
