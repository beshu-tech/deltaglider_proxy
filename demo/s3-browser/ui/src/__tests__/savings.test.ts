import assert from 'node:assert/strict';
import { describe, expect, test } from 'vitest';
import {
  summarizeScopeSavings,
  summarizeObjectSavings,
  GIB,
  gibFromBytes,
  bytesFromGib,
  monthlyCost,
  parseCostRate,
  DEFAULT_COST_RATE,
  uploadCreatedBaseline,
  savingsColors,
  heroPresentation,
  biggestSaveSentence,
} from '../savings';

test('summarizeScopeSavings', () => {
  assert.deepEqual(summarizeScopeSavings(0, 0), { pct: 0, pctOneDecimal: 0, savedBytes: 0, empty: true });
  assert.deepEqual(summarizeScopeSavings(1000, 1200), { pct: 0, pctOneDecimal: 0, savedBytes: 0, empty: false });
  assert.deepEqual(summarizeScopeSavings(1000, 104), { pct: 89, pctOneDecimal: 89.6, savedBytes: 896, empty: false });
  // 99.95% raw never reads 100
  const scope9995 = summarizeScopeSavings(100000, 5);
  assert.equal(scope9995.pct, 99);
  assert.equal(scope9995.pctOneDecimal, 99.9);
});

test('summarizeObjectSavings', () => {
  assert.deepEqual(summarizeObjectSavings(1000, null), { pct: 0, savedBytes: 0, empty: true });
  assert.deepEqual(summarizeObjectSavings(0, 0), { pct: 0, savedBytes: 0, empty: true });
  assert.equal(summarizeObjectSavings(1000, 1200).pct, 0, 'negative savings clamp to 0');
  // the rendered string must never be "100.0%" while bytes remain on disk:
  // 99.95..99.99 previously passed the >=100 cap and toFixed(1) rounded up.
  for (const stored of [1, 3, 5]) {
    const { pct } = summarizeObjectSavings(100000, stored); // 99.999, 99.997, 99.995
    assert.ok(pct <= 99.9, `stored=${stored}: pct ${pct} must cap at 99.9`);
    assert.notEqual(pct.toFixed(1), '100.0', `stored=${stored} renders 100.0%`);
  }
  assert.equal(summarizeObjectSavings(1000, 104).pct, 89.6);
  // floor, not round: 89.66 → 89.6
  assert.equal(summarizeObjectSavings(10000, 1034).pct, 89.6);
  // only a literally-empty stored object is an honest 100%
  assert.equal(summarizeObjectSavings(1000, 0).pct, 100);
});

test('GiB quota conversion (BucketCard quota input)', () => {
  assert.equal(GIB, 1024 ** 3);
  // 500 MiB must not read as 0 GiB
  assert.equal(gibFromBytes(500 * 1024 ** 2), 0.488);
  assert.equal(gibFromBytes(2 * GIB), 2);
  assert.equal(gibFromBytes(1.5 * GIB), 1.5);
  // write path: always whole bytes (quota_bytes is a u64 server-side)
  assert.equal(bytesFromGib(0.5), 512 * 1024 ** 2);
  assert.equal(bytesFromGib(2), 2 * GIB);
  assert.ok(Number.isInteger(bytesFromGib(0.1)), '0.1 GiB → integer bytes');
  assert.equal(bytesFromGib(0.1), 107374182);
  // round-trip of a typed decimal stays stable
  assert.equal(gibFromBytes(bytesFromGib(0.3)), 0.3);
});

test('monthlyCost: bill = (bytes / GiB) × $/GB/mo', () => {
  assert.equal(monthlyCost(0, 0.023), 0);
  assert.equal(monthlyCost(GIB, 0.023), 0.023);
  assert.equal(monthlyCost(10 * GIB, 0.5), 5);
  assert.equal(monthlyCost(GIB / 2, 1), 0.5);
});

test('parseCostRate: persisted $/GB/mo from localStorage', () => {
  assert.equal(DEFAULT_COST_RATE, 0.00524);
  assert.equal(parseCostRate('0.023'), 0.023);
  assert.equal(parseCostRate('0'), 0, 'free storage is a valid rate');
  // missing / corrupt / nonsensical → default, never NaN costs
  for (const raw of [null, undefined, '', 'abc', 'NaN', 'Infinity', '-1']) {
    assert.equal(parseCostRate(raw), DEFAULT_COST_RATE, `raw=${String(raw)}`);
  }
});

test('uploadCreatedBaseline: only the write response that says so', () => {
  // The proxy sends `x-deltaglider-baseline: created` only on the write that
  // created the folder's reference; a re-upload of the same bytes does not.
  assert.equal(uploadCreatedBaseline({ 'x-deltaglider-baseline': 'created', 'x-amz-storage-type': 'delta' }), true);
  assert.equal(uploadCreatedBaseline({ 'x-amz-storage-type': 'delta' }), false, 'a delta against an existing baseline');
  assert.equal(uploadCreatedBaseline({ 'x-amz-storage-type': 'passthrough' }), false);
  assert.equal(uploadCreatedBaseline({}), false, 'no facts: do not guess');
});

describe('savings presentation (UX harvest D14, D15, D49)', () => {
  test('one green for every saving', () => {
    expect(savingsColors({ ACCENT_GREEN: '#047857' })).toEqual({ text: '#047857', bg: '#04785714', border: '#04785740' });
  });
  test('an estimate is neutral with ≈, a scanned total is green', () => {
    expect(heroPresentation(2)).toMatchObject({ estimated: true, prefix: '≈', tone: 'neutral' });
    expect(heroPresentation(0)).toMatchObject({ estimated: false, prefix: '', tone: 'saved' });
  });
  test('the biggest saving has no minus sign', () => {
    const s = biggestSaveSentence('releases', '7.6 MiB');
    expect(s).toBe('releases saves the most: 7.6 MiB');
    expect(s).not.toMatch(/[−-]\s*7/);
  });
});
