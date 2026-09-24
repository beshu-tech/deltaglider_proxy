import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile savings.ts (zero deps) to an importable data: URL.
const source = await readFile(new URL('../src/savings.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'savings.ts',
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
const { summarizeScopeSavings, summarizeObjectSavings, GIB, gibFromBytes, bytesFromGib } = await import(moduleUrl);

// --- summarizeScopeSavings ---------------------------------------------------
assert.deepEqual(summarizeScopeSavings(0, 0), { pct: 0, pctOneDecimal: 0, savedBytes: 0, empty: true });
assert.deepEqual(summarizeScopeSavings(1000, 1200), { pct: 0, pctOneDecimal: 0, savedBytes: 0, empty: false });
assert.deepEqual(summarizeScopeSavings(1000, 104), { pct: 89, pctOneDecimal: 89.6, savedBytes: 896, empty: false });
// 99.95% raw never reads 100
const scope9995 = summarizeScopeSavings(100000, 5);
assert.equal(scope9995.pct, 99);
assert.equal(scope9995.pctOneDecimal, 99.9);

// --- summarizeObjectSavings --------------------------------------------------
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

// --- GiB quota conversion (BucketCard quota input) ---------------------------
assert.equal(GIB, 1024 ** 3);
// 500 MiB must not read as 0 GB
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

console.log('savings regression checks passed');
