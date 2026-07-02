import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// permissionActions.ts imports parseResourcePattern from ../storagePath; rewrite
// that bare import to a pre-built data URL so the graph resolves without a bundler.
async function loadModule(relPath, fileName, replaceImports = {}) {
  let source = await readFile(new URL(relPath, import.meta.url), 'utf8');
  for (const [spec, dataUrl] of Object.entries(replaceImports)) {
    source = source.replaceAll(`'${spec}'`, `'${dataUrl}'`);
  }
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const storagePathUrl = await loadModule('../src/storagePath.ts', 'storagePath.ts');
const modUrl = await loadModule('../src/components/permissionActions.ts', 'permissionActions.ts', {
  '../storagePath': storagePathUrl,
});
const { effectiveActions, toggleAction, grantSummary, reconcileActionsForScope, isPrefixScoped } = await import(modUrl);

const set = (arr) => [...effectiveActions(arr)].sort();

// effectiveActions: "*" expands to all five atomics; junk is dropped.
assert.deepEqual(set(['*']), ['admin', 'delete', 'list', 'read', 'write']);
assert.deepEqual(set(['read', 'list']), ['list', 'read']);
assert.deepEqual(set([]), []);
assert.deepEqual(set(['read', 'bogus', 's3:GetObject']), ['read']); // unknowns ignored

// toggleAction: add a chip.
assert.deepEqual(toggleAction([], 'read'), ['read']);
assert.deepEqual(toggleAction(['read'], 'list'), ['list', 'read']); // stable order list<read
assert.deepEqual(toggleAction(['list'], 'read'), ['list', 'read']);

// write WITHOUT delete is expressible (the whole point — no ladder).
assert.deepEqual(toggleAction(['read', 'list'], 'write'), ['list', 'read', 'write']);
assert.ok(!toggleAction(['read', 'list'], 'write').includes('delete'));

// Remove a chip.
assert.deepEqual(toggleAction(['list', 'read', 'write'], 'write'), ['list', 'read']);

// Collapse to "*" once all five are present.
assert.deepEqual(toggleAction(['list', 'read', 'write', 'delete'], 'admin'), ['*']);

// Toggling OFF from "*" expands to the explicit remaining four in canonical
// atomic order (list, read, write, delete, admin) — never a partial "*".
assert.deepEqual(toggleAction(['*'], 'admin'), ['list', 'read', 'write', 'delete']);
assert.deepEqual(toggleAction(['*'], 'delete'), ['list', 'read', 'write', 'admin']);

// Order is always the canonical atomic order, regardless of input order.
assert.deepEqual(toggleAction(['admin', 'write'], 'list'), ['list', 'write', 'admin']);

// grantSummary — plain-language captions.
assert.equal(grantSummary([]), 'No actions selected — this grant does nothing.');
assert.equal(grantSummary(['*']), 'Full control, including bucket-level operations. Grants *.');
assert.equal(grantSummary(['list', 'read']), 'Browse & download. Grants list, read.');
assert.equal(grantSummary(['read', 'write', 'list']), 'Browse, download & upload. Grants list, read, write.');
assert.equal(grantSummary(['list']), 'Browse. Grants list.');
// write-without-delete reads correctly.
assert.equal(grantSummary(['list', 'read', 'write']), 'Browse, download & upload. Grants list, read, write.');

// reconcileActionsForScope — the guard for "narrowed scope must drop admin".
// At bucket scope: no-op (admin allowed).
assert.deepEqual(reconcileActionsForScope(['list', 'admin'], false), ['list', 'admin']);
assert.deepEqual(reconcileActionsForScope(['*'], false), ['*']);
// Prefix scope WITHOUT admin: unchanged (referential return is fine either way).
assert.deepEqual(reconcileActionsForScope(['list', 'read'], true), ['list', 'read']);
// THE bug: prefix scope WITH admin strips admin.
assert.deepEqual(reconcileActionsForScope(['list', 'admin'], true), ['list']);
assert.deepEqual(reconcileActionsForScope(['read', 'write', 'admin'], true), ['read', 'write']);
// A collapsed "*" at prefix scope expands to the four non-admin actions.
assert.deepEqual(reconcileActionsForScope(['*'], true), ['list', 'read', 'write', 'delete']);
// Idempotent: re-running on an already-clean prefix grant changes nothing.
assert.deepEqual(reconcileActionsForScope(['list', 'read', 'write', 'delete'], true), ['list', 'read', 'write', 'delete']);
// Admin-ONLY at prefix scope reconciles to [] (the rule then has no actions and
// is dropped on save — the editor surfaces this as an "incomplete rule" warning).
assert.deepEqual(reconcileActionsForScope(['admin'], true), []);
assert.deepEqual(reconcileActionsForScope([], true), []);
// Referential stability: a no-op MUST return the SAME array reference (the load-
// time normalize effect relies on `next !== prev` to avoid a render loop).
{
  const r = ['list', 'read'];
  assert.ok(reconcileActionsForScope(r, true) === r, 'prefix no-admin returns same ref');
  assert.ok(reconcileActionsForScope(r, false) === r, 'bucket scope returns same ref');
}

// grantSummary — admin verb paths.
assert.equal(grantSummary(['admin']), 'Manage buckets. Grants admin.');
assert.equal(grantSummary(['write', 'admin']), 'Upload & manage buckets. Grants write, admin.');

// isPrefixScoped — the truth table that drives the Admin-chip guard.
// Now takes a string[] (resources are an array end-to-end, never comma-joined).
assert.equal(isPrefixScoped(['beshu']), false, 'bucket only → admin offered');
assert.equal(isPrefixScoped(['beshu/*']), false, 'bucket wildcard → admin offered');
assert.equal(isPrefixScoped(['beshu/ror/*']), true, 'sub-prefix → admin suppressed');
assert.equal(isPrefixScoped(['*']), false, 'global → admin offered');
assert.equal(isPrefixScoped([]), false, 'empty → false');
assert.equal(isPrefixScoped(['   ']), false, 'whitespace-only → false');
// Mixed: any bucket-level resource → admin offered (.every short-circuits).
assert.equal(isPrefixScoped(['beshu/ror/*', 'beshu']), false, 'mixed with a bucket → admin offered');
assert.equal(isPrefixScoped(['beshu/ror/*', 'beshu/x/*']), true, 'all sub-prefix → suppressed');
assert.equal(isPrefixScoped(['beshu/ror/*', '*']), false, 'any global → admin offered');
// Template bucket: with a prefix → suppressed (privilege-safe); without → offered.
assert.equal(isPrefixScoped(['${iam:username}/x/*']), true, 'templated bucket + prefix → suppressed');
assert.equal(isPrefixScoped(['${iam:username}/*']), false, 'templated bucket, no prefix → offered');

console.log('action chips regression checks passed');
