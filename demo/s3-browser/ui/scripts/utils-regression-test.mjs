import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile utils.ts (zero deps) to an importable data: URL.
const source = await readFile(new URL('../src/utils.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ES2020,
    target: ts.ScriptTarget.ES2020,
    importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
  },
  fileName: 'utils.ts',
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
const { clamp, formatDuration, relativeTime, formatBytes, getFileName, pluralize, parentPrefix } = await import(moduleUrl);

// --- clamp -------------------------------------------------------------------
assert.equal(clamp(50, 0, 100), 50);
assert.equal(clamp(-10, 0, 100), 0);
assert.equal(clamp(140, 0, 100), 100);
assert.equal(clamp(0, 0, 100), 0);
assert.equal(clamp(100, 0, 100), 100);
// non-finite collapses to the lower bound
assert.equal(clamp(NaN, 0, 100), 0);
assert.equal(clamp(Infinity, 0, 100), 0);
assert.equal(clamp(Infinity, -5, 5), -5);
assert.equal(clamp(-Infinity, -5, 5), -5);
// arbitrary bounds (used by DeltaEfficiencyPanel's [0,100] axis mapping)
assert.equal(clamp(3, 1, 2), 2);
assert.equal(clamp(0.5, 1, 2), 1);

// --- formatDuration + relativeTime (the ONE relative-time vocabulary) --------
// Units: s / m / h (+ m) / d / mo / y. Past: "<dur> ago". Future: "in <dur>"
// only when asked (scheduled times); otherwise a future instant is clock skew
// and reads "just now".
assert.equal(formatDuration(0), '0s');
assert.equal(formatDuration(47), '47s');
assert.equal(formatDuration(90), '1m');
assert.equal(formatDuration(59 * 60 + 59), '59m');
assert.equal(formatDuration(2 * 3600), '2h');
assert.equal(formatDuration(3 * 3600 + 21 * 60), '3h 21m');
assert.equal(formatDuration(3 * 86400 + 5 * 3600), '3d');
assert.equal(formatDuration(45 * 86400), '1mo');
assert.equal(formatDuration(800 * 86400), '2y');
assert.equal(formatDuration(-5), '0s', 'negative clamps to 0');

const NOW = Date.UTC(2026, 0, 15, 12, 0, 0);
const ago = (ms) => new Date(NOW - ms).toISOString();
assert.equal(relativeTime(ago(0), { now: NOW }), 'just now');
assert.equal(relativeTime(ago(5_000), { now: NOW }), '5s ago');
assert.equal(relativeTime(ago(90_000), { now: NOW }), '1m ago');
assert.equal(relativeTime(ago(2 * 3600_000), { now: NOW }), '2h ago');
assert.equal(relativeTime(ago(2 * 3600_000 + 21 * 60_000), { now: NOW }), '2h 21m ago');
assert.equal(relativeTime(ago(3 * 86400_000), { now: NOW }), '3d ago');
assert.equal(relativeTime(ago(400 * 86400_000), { now: NOW }), '1y ago');
// accepts Date, ISO string, epoch ms; `now` as Date or ms
assert.equal(relativeTime(new Date(NOW - 5_000), { now: new Date(NOW) }), '5s ago');
assert.equal(relativeTime(NOW - 5_000, { now: NOW }), '5s ago');
// future: clock skew → "just now" by default, "in 5m" when scheduled
assert.equal(relativeTime(NOW + 300_000, { now: NOW }), 'just now');
assert.equal(relativeTime(NOW + 300_000, { now: NOW, future: true }), 'in 5m');
assert.equal(relativeTime(NOW - 300_000, { now: NOW, future: true }), '5m ago');
// missing / unparseable
assert.equal(relativeTime(null), '—');
assert.equal(relativeTime(undefined), '—');
assert.equal(relativeTime('not a date'), '—');

// --- formatBytes (regression guard for the shared analytics formatter) -------
assert.equal(formatBytes(0), '0 B');
assert.equal(formatBytes(512), '512 B');
assert.equal(formatBytes(1536), '1.5 KB');
// sign-aware: a negative delta (stored > original) must not render "NaN undefined"
assert.equal(formatBytes(-5), '-5 B');
assert.equal(formatBytes(-1536), '-1.5 KB');
// fractional sub-byte input clamps to the B unit instead of units[-1]
assert.equal(formatBytes(0.5), '1 B');
// PB exists, and the unit index clamps at the largest unit
assert.equal(formatBytes(2 * 1024 ** 5), '2.0 PB');
assert.equal(formatBytes(2048 * 1024 ** 5), '2048.0 PB');
// non-finite input never reaches the log math
assert.equal(formatBytes(NaN), '—');
assert.equal(formatBytes(Infinity), '—');

// --- getFileName (shared filename extraction for Inspector + Preview) ---------
assert.equal(getFileName('a/b/c.txt'), 'c.txt');
assert.equal(getFileName('flat.bin'), 'flat.bin');
assert.equal(getFileName('deep/nested/path/'), 'deep/nested/path/'); // trailing slash -> falls back to key
assert.equal(getFileName(''), '');
assert.equal(getFileName('no-slash'), 'no-slash');

// --- pluralize ---------------------------------------------------------------
assert.equal(pluralize(1, 'item'), '1 item');
assert.equal(pluralize(0, 'item'), '0 items');
assert.equal(pluralize(3, 'item'), '3 items');
assert.equal(pluralize(2, 'entry', 'entries'), '2 entries');
assert.equal(pluralize(1, 'entry', 'entries'), '1 entry');

// --- parentPrefix (keyboard "up a folder" navigation) ------------------------
assert.equal(parentPrefix(''), '', 'root stays root');
assert.equal(parentPrefix('a/'), '', 'one level up from top → root');
assert.equal(parentPrefix('a/b/'), 'a/', 'nested → parent with trailing slash');
assert.equal(parentPrefix('a/b/c/'), 'a/b/', 'deep nested → immediate parent');
// tolerates a missing trailing slash (defensive — callers pass prefixes that
// normally end in "/", but a stray non-slashed prefix must not throw)
assert.equal(parentPrefix('a/b'), 'a/', 'no trailing slash still climbs one');
assert.equal(parentPrefix('single'), '', 'single segment, no slash → root');

console.log('utils regression checks passed');
