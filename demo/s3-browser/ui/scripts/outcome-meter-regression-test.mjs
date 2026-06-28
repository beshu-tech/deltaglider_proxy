// Regression test for deriveMeter (the pure decision fn behind OutcomeMeter,
// living in jobsView.ts alongside the other job-display helpers).
// Mirrors scripts/jobs-view-regression-test.mjs: transpile + import the TS.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/jobsView.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'jobsView.ts',
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
const { deriveMeter } = await import(moduleUrl);

const base = { scanned: 0, copied: 0, errors: 0, skipped: 0, status: 'succeeded', percent: null };
const m = (over) => deriveMeter({ ...base, ...over });

// in-sync: everything skipped (the dominant, calm case) — NOT saturated green.
{
  const v = m({ scanned: 11, skipped: 11, status: 'succeeded' });
  assert.equal(v.state, 'in-sync');
  assert.equal(v.dot, 'muted');
  assert.equal(v.label, 'in sync');
  assert.equal(v.greenPct, 0, 'in-sync must not paint green');
}

// empty: nothing scanned at all.
{
  const v = m({ scanned: 0, status: 'succeeded' });
  assert.equal(v.state, 'in-sync');
  assert.equal(v.label, 'no objects');
}

// copied only.
{
  const v = m({ scanned: 8, copied: 8, skipped: 0, status: 'succeeded' });
  assert.equal(v.state, 'copied');
  assert.equal(v.dot, 'green');
  assert.equal(v.greenPct, 100);
  assert.equal(v.redPct, 0);
  assert.equal(v.label, '8 copied');
}

// errors only.
{
  const v = m({ scanned: 5, errors: 5, status: 'succeeded' });
  assert.equal(v.state, 'errors');
  assert.equal(v.dot, 'red');
  assert.equal(v.redPct, 100);
  assert.equal(v.label, '5 errors');
  assert.equal(m({ scanned: 1, errors: 1, status: 'succeeded' }).label, '1 error', 'singular');
}

// mixed: errors own the dot, proportions honest.
{
  const v = m({ scanned: 100, copied: 75, errors: 25, status: 'succeeded' });
  assert.equal(v.state, 'mixed');
  assert.equal(v.dot, 'red', 'errors own the attention dot');
  assert.equal(v.greenPct, 75);
  assert.equal(v.redPct, 25);
  assert.equal(v.label, '75 copied · 25 err');
}

// failed with nothing acted on → loud red.
{
  const v = m({ scanned: 58, errors: 0, copied: 0, skipped: 0, status: 'failed' });
  assert.equal(v.state, 'errors');
  assert.equal(v.redPct, 100);
  assert.equal(v.label, 'failed');
}

// failed but with a real error count → uses the error label/proportions.
{
  const v = m({ scanned: 58, errors: 58, status: 'failed' });
  assert.equal(v.label, '58 errors');
}

// running, known percent → green fill + amber dot.
{
  const v = m({ scanned: 50, copied: 20, status: 'running', percent: 40 });
  assert.equal(v.state, 'running');
  assert.equal(v.dot, 'amber');
  assert.equal(v.greenPct, 40);
  assert.equal(v.label, 'running · 20 copied');
}

// running, unknown percent → indeterminate (green 0), trailing ellipsis label.
{
  const v = m({ scanned: 50, copied: 20, status: 'running', percent: null });
  assert.equal(v.state, 'running');
  assert.equal(v.greenPct, 0);
  assert.equal(v.label, 'running · 20 copied…');
}

// queued / cancelling also read as running.
assert.equal(m({ status: 'queued' }).state, 'running');
assert.equal(m({ status: 'cancelling' }).state, 'running');

// cancelled → calm muted.
{
  const v = m({ scanned: 10, copied: 3, status: 'cancelled' });
  assert.equal(v.state, 'idle');
  assert.equal(v.dot, 'muted');
  assert.equal(v.label, 'cancelled');
}

// huge numbers → thousands separators.
{
  const v = m({ scanned: 4_000_000, copied: 3_883_000, errors: 58, status: 'succeeded' });
  assert.equal(v.label, '3,883,000 copied · 58 err');
}

// aria always carries the full breakdown.
{
  const v = m({ scanned: 8, copied: 8, status: 'succeeded' });
  assert.match(v.aria, /8 scanned, 8 copied, 0 skipped/);
}

console.log('outcome-meter regression test: OK');
