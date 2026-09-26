/** src/components/scanFreshness.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { scanAgeMs, isScanStale, SCAN_STALE_MS } from '../components/scanFreshness';

// fixed "now" for determinism
const NOW = Date.parse('2026-05-31T12:00:00Z');
const iso = (msAgo: number) => new Date(NOW - msAgo).toISOString();

test('SCAN_STALE_MS is 6 hours', () => {
  assert.equal(SCAN_STALE_MS, 6 * 60 * 60 * 1000);
});

test('scanAgeMs', () => {
  assert.equal(scanAgeMs(null, NOW), null);
  assert.equal(scanAgeMs(undefined, NOW), null);
  assert.equal(scanAgeMs('not-a-date', NOW), null);
  assert.equal(scanAgeMs(iso(0), NOW), 0);
  assert.equal(scanAgeMs(iso(5000), NOW), 5000);
  // future timestamp (clock skew) clamps to 0, never negative
  assert.equal(scanAgeMs(new Date(NOW + 10_000).toISOString(), NOW), 0);
});

test('isScanStale: 6h boundary', () => {
  assert.equal(isScanStale(iso(0), undefined, NOW), false);
  assert.equal(isScanStale(iso(5 * 3600_000), undefined, NOW), false); // 5h fresh
  assert.equal(isScanStale(iso(6 * 3600_000), undefined, NOW), false); // exactly 6h: not yet stale (> not >=)
  assert.equal(isScanStale(iso(6 * 3600_000 + 1), undefined, NOW), true); // just past 6h
  assert.equal(isScanStale(iso(24 * 3600_000), undefined, NOW), true); // 1 day stale
  // custom ttl
  assert.equal(isScanStale(iso(2000), 1000, NOW), true);
  assert.equal(isScanStale(iso(500), 1000, NOW), false);
  // missing → never stale
  assert.equal(isScanStale(null, undefined, NOW), false);
  assert.equal(isScanStale('garbage', undefined, NOW), false);
});
