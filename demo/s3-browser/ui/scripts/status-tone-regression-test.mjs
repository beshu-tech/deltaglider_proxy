import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/statusTone.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'statusTone.ts',
});
const { countTone, eventStatusTone, serverErrorSeverity } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

// A zero counter is neutral, whatever its category (issue #92 item 11: red "0").
assert.equal(countTone(0, 'error'), 'default');
assert.equal(countTone(0, 'warning'), 'default');
assert.equal(countTone(0, 'success'), 'default');
assert.equal(countTone(7, 'error'), 'error');
assert.equal(countTone(1, 'warning'), 'warning');

// Event rows: only failures are red; a fresh pending event is neutral.
assert.equal(eventStatusTone('failed', 8), 'error');
assert.equal(eventStatusTone('delivered', 1), 'success');
assert.equal(eventStatusTone('in_progress', 0), 'processing');
assert.equal(eventStatusTone('pending', 0), 'default');
assert.equal(eventStatusTone('pending', 3), 'warning');

// Dashboard error rate: 4xx alone (HEAD-before-PUT 404s) never alarms.
assert.equal(serverErrorSeverity(0, 534), 'good');
assert.equal(serverErrorSeverity(0, 0), 'good');
assert.equal(serverErrorSeverity(3, 100), 'warn');
assert.equal(serverErrorSeverity(6, 100), 'bad');
assert.equal(serverErrorSeverity(1, 100), 'good');

// Source guard: the event log counters go through countTone, so a zero
// count can never render as an alarm again.
const panel = await readFile(new URL('../src/components/EventOutboxPanel.tsx', import.meta.url), 'utf8');
assert.ok(panel.includes('countTone('), 'EventOutboxPanel must colour its counters with countTone');
assert.ok(!panel.includes('colour="error"'), 'EventOutboxPanel must not hard-code a red counter');

console.log('status-tone regression tests passed');
