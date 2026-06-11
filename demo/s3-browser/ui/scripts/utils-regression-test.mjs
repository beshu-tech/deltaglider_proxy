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
const { clamp, dotPattern, ageLabel, formatBytes, getFileName, pluralize, parentPrefix } = await import(moduleUrl);

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

// --- dotPattern --------------------------------------------------------------
const pat = dotPattern('#abc123');
assert.ok(pat.startsWith('url("data:image/svg+xml;utf8,'), 'is a data-URL background');
assert.ok(pat.endsWith('")'), 'is a closed url() wrapper');
assert.ok(pat.includes(encodeURIComponent('#abc123')), 'colour is URI-encoded');
// SVG markup uses single quotes so the url() double-quote wrapper stays valid
assert.ok(pat.includes("<svg xmlns='http://www.w3.org/2000/svg'"), 'svg attrs single-quoted');
// deterministic — same input, same output
assert.equal(dotPattern('#abc123'), pat);

// --- ageLabel ----------------------------------------------------------------
assert.equal(ageLabel(null), 'never');
const now = Date.now();
assert.equal(ageLabel(new Date(now - 5_000).toISOString()), '5s ago');
assert.equal(ageLabel(new Date(now - 90_000).toISOString()), '1m ago');
assert.equal(ageLabel(new Date(now - 2 * 3600_000).toISOString()), '2h ago');
assert.equal(ageLabel(new Date(now - (2 * 3600_000 + 21 * 60_000)).toISOString()), '2h 21m ago');
assert.equal(ageLabel(new Date(now - 3 * 86400_000).toISOString()), '3d ago');
// future timestamp clamps to "just now"
assert.equal(ageLabel(new Date(now + 10_000).toISOString()), 'just now');

// --- formatBytes (regression guard for the shared analytics formatter) -------
assert.equal(formatBytes(0), '0 B');
assert.equal(formatBytes(512), '512 B');
assert.equal(formatBytes(1536), '1.5 KB');

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
