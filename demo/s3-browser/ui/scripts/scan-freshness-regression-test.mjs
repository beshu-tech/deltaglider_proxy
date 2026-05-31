import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const sourceUrl = new URL('../src/components/scanFreshness.ts', import.meta.url);
const source = await readFile(sourceUrl, 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'scanFreshness.ts',
});
const mod = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);
const { scanAgeMs, isScanStale, scanAgeLabel, SCAN_STALE_MS } = mod;

// fixed "now" for determinism
const NOW = Date.parse('2026-05-31T12:00:00Z');
const iso = (msAgo) => new Date(NOW - msAgo).toISOString();

assert.equal(SCAN_STALE_MS, 6 * 60 * 60 * 1000);

// --- scanAgeMs ---------------------------------------------------------------
assert.equal(scanAgeMs(null, NOW), null);
assert.equal(scanAgeMs(undefined, NOW), null);
assert.equal(scanAgeMs('not-a-date', NOW), null);
assert.equal(scanAgeMs(iso(0), NOW), 0);
assert.equal(scanAgeMs(iso(5000), NOW), 5000);
// future timestamp (clock skew) clamps to 0, never negative
assert.equal(scanAgeMs(new Date(NOW + 10_000).toISOString(), NOW), 0);

// --- isScanStale: 6h boundary ------------------------------------------------
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

// --- scanAgeLabel ------------------------------------------------------------
assert.equal(scanAgeLabel(null, NOW), '');
assert.equal(scanAgeLabel(iso(10_000), NOW), 'just now'); // <45s
assert.equal(scanAgeLabel(iso(5 * 60_000), NOW), '5m ago');
assert.equal(scanAgeLabel(iso(59 * 60_000), NOW), '59m ago');
assert.equal(scanAgeLabel(iso(3 * 3600_000), NOW), '3h ago');
assert.equal(scanAgeLabel(iso(23 * 3600_000), NOW), '23h ago');
assert.equal(scanAgeLabel(iso(2 * 24 * 3600_000), NOW), '2d ago');

console.log('scan freshness regression checks passed');
