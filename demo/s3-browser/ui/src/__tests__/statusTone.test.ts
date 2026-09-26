import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'vitest';
import { countTone, eventStatusTone, serverErrorSeverity, cacheMissSeverity, endpointDeliverySummary } from '../statusTone';

test('countTone: a zero counter is neutral, whatever its category', () => {
  // A zero counter is neutral, whatever its category (issue #92 item 11: red "0").
  assert.equal(countTone(0, 'error'), 'default');
  assert.equal(countTone(0, 'warning'), 'default');
  assert.equal(countTone(0, 'success'), 'default');
  assert.equal(countTone(7, 'error'), 'error');
  assert.equal(countTone(1, 'warning'), 'warning');
});

test('eventStatusTone: only failures are red; a fresh pending event is neutral', () => {
  assert.equal(eventStatusTone('failed', 8), 'error');
  assert.equal(eventStatusTone('delivered', 1), 'success');
  assert.equal(eventStatusTone('in_progress', 0), 'processing');
  assert.equal(eventStatusTone('pending', 0), 'default');
  assert.equal(eventStatusTone('pending', 3), 'warning');
});

test('endpointDeliverySummary: a summary chip only when an endpoint was attempted', () => {
  assert.equal(endpointDeliverySummary([]), null);
  assert.deepEqual(endpointDeliverySummary([{ status: 'delivered' }, { status: 'delivered' }]), {
    text: '2/2 endpoints',
    tone: 'success',
  });
  assert.deepEqual(endpointDeliverySummary([{ status: 'delivered' }, { status: 'failed' }]), {
    text: '1/2 endpoints',
    tone: 'warning',
  });
  assert.deepEqual(endpointDeliverySummary([{ status: 'failed' }]), { text: '0/1 endpoint', tone: 'error' });
});

test('serverErrorSeverity: 4xx alone (HEAD-before-PUT 404s) never alarms', () => {
  assert.equal(serverErrorSeverity(0, 534), 'good');
  assert.equal(serverErrorSeverity(0, 0), 'good');
  assert.equal(serverErrorSeverity(3, 100), 'warn');
  assert.equal(serverErrorSeverity(6, 100), 'bad');
  assert.equal(serverErrorSeverity(1, 100), 'good');
});

test('cacheMissSeverity: a cold start is not an alarm', () => {
  assert.equal(cacheMissSeverity(1, 1), 'neutral');
  assert.equal(cacheMissSeverity(1, 19), 'neutral');
  assert.equal(cacheMissSeverity(0.6, 20), 'bad');
  assert.equal(cacheMissSeverity(0.3, 100), 'warn');
  assert.equal(cacheMissSeverity(0.1, 100), 'good');
});

test('source guard: the dashboard error card colours what it shows', async () => {
  // Source guard: the dashboard error card shows the 5xx share, the same
  // number its colour follows (issue #92 review: "50 %" rendered green).
  const metrics = await readFile(new URL('../components/MetricsPage.tsx', import.meta.url), 'utf8');
  assert.ok(!/\['4xx'\][^\n]*\+[^\n]*\['5xx'\]/.test(metrics), 'MetricsPage must not add 4xx to the headline error rate');
  assert.ok(metrics.includes('cacheMissSeverity('), 'MetricsPage must colour the cache hit rate with cacheMissSeverity');
});

test('source guard: the event log counters go through countTone', async () => {
  // Source guard: the event log counters go through countTone, so a zero
  // count can never render as an alarm again.
  const panel = await readFile(new URL('../components/EventOutboxPanel.tsx', import.meta.url), 'utf8');
  assert.ok(panel.includes('countTone('), 'EventOutboxPanel must colour its counters with countTone');
  assert.ok(!panel.includes('colour="error"'), 'EventOutboxPanel must not hard-code a red counter');
});
