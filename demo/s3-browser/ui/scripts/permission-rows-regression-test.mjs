import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile a TS module to an importable data: URL. `replaceImports` rewrites
// bare relative imports to already-built data URLs so the dependency graph
// (permissionRows -> storagePath) resolves without a bundler. The `adminApi`
// import in permissionRows is type-only and erased by transpilation.
async function loadModule(relPath, fileName, replaceImports = {}) {
  const url = new URL(relPath, import.meta.url);
  let source = await readFile(url, 'utf8');
  for (const [spec, dataUrl] of Object.entries(replaceImports)) {
    source = source.replaceAll(`'${spec}'`, `'${dataUrl}'`);
  }
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ES2020,
      target: ts.ScriptTarget.ES2020,
      importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
    },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const storagePathUrl = await loadModule('../src/storagePath.ts', 'storagePath.ts');
const rowsUrl = await loadModule('../src/components/permissionRows.ts', 'permissionRows.ts', {
  '../storagePath': storagePathUrl,
});

const { permissionsToRows, rowsToPermissions, freshPermissionRowId } = await import(rowsUrl);

// --- freshPermissionRowId: stable, unique ------------------------------------
const a = freshPermissionRowId();
const b = freshPermissionRowId();
assert.notEqual(a, b);
assert.match(a, /^perm-\d+$/);

// --- permissionsToRows assigns a stable _uiId to every row -------------------
const perms = [
  { id: 1, effect: 'Allow', actions: ['read', 'list'], resources: ['bucket/*'] },
  { id: 2, effect: 'Deny', actions: ['delete'], resources: ['bucket/secret/*'], conditions: { IpAddress: { 'aws:SourceIp': ['10.0.0.0/8'] } } },
];
const rows = permissionsToRows(perms);
assert.equal(rows.length, 2);
assert.ok(rows[0]._uiId, 'row 0 must have a _uiId');
assert.ok(rows[1]._uiId, 'row 1 must have a _uiId');
assert.notEqual(rows[0]._uiId, rows[1]._uiId, 'ids must be unique per row');
// fields carried through
assert.equal(rows[0].resources, 'bucket/*');
assert.deepEqual(rows[1].actions, ['delete']);
assert.deepEqual(rows[1].conditions, { IpAddress: { 'aws:SourceIp': ['10.0.0.0/8'] } });

// --- THE REGRESSION: _uiId NEVER leaks into the wire IamPermission ------------
const wire = rowsToPermissions(rows);
assert.equal(wire.length, 2);
for (const p of wire) {
  assert.ok(!('_uiId' in p), '_uiId must be stripped from the wire permission');
}
// wire content is correct (resources normalized + split)
assert.deepEqual(wire[0].resources, ['bucket/*']);
assert.equal(wire[0].effect, 'Allow');
assert.deepEqual(wire[1].actions, ['delete']);
assert.deepEqual(wire[1].conditions, { IpAddress: { 'aws:SourceIp': ['10.0.0.0/8'] } });

// --- rows with a manually-attached _uiId also strip cleanly ------------------
const manual = rowsToPermissions([
  { _uiId: 'perm-999', effect: 'Allow', actions: ['read'], resources: 'b/*' },
]);
assert.equal(manual.length, 1);
assert.ok(!('_uiId' in manual[0]));
assert.deepEqual(manual[0].resources, ['b/*']);

// --- empty/incomplete rows are dropped (unchanged contract) ------------------
const dropped = rowsToPermissions([
  { _uiId: 'perm-1', effect: 'Allow', actions: [], resources: 'b/*' }, // no actions
  { _uiId: 'perm-2', effect: 'Allow', actions: ['read'], resources: '  ' }, // blank resources
  { _uiId: 'perm-3', effect: 'Allow', actions: ['read'], resources: 'b/*' }, // keep
]);
assert.equal(dropped.length, 1);
assert.deepEqual(dropped[0].resources, ['b/*']);

console.log('permission rows regression checks passed');
