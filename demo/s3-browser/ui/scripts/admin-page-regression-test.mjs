import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// --- Every ADMIN_IA leaf renders a page -------------------------------------
// adminNavigation.tsx and adminRoutes.tsx carry JSX (icons, panels), so read
// the path literals from source instead of importing them. A leaf with no
// route silently fell through to the dashboard; a route with no leaf is dead.
const iaSrc = await readFile(new URL('../src/components/adminNavigation.tsx', import.meta.url), 'utf8');
const iaPaths = [...iaSrc.matchAll(/^\s+path: '([^']+)',$/gm)].map((m) => m[1]);
assert.ok(iaPaths.length >= 17, `expected the admin IA leaves, got ${iaPaths.length}`);

const routesSrc = await readFile(new URL('../src/components/admin/adminRoutes.tsx', import.meta.url), 'utf8');
const table = routesSrc.slice(routesSrc.indexOf('const ADMIN_ROUTES'));
const routePaths = [...table.matchAll(/^ {2}'([^']+)': \{$/gm)].map((m) => m[1]);

for (const p of iaPaths) assert.ok(routePaths.includes(p), `ADMIN_IA leaf '${p}' has no route`);
// `setup` is reachable by URL and the palette, not the sidebar.
for (const p of routePaths) {
  assert.ok(p === 'setup' || iaPaths.includes(p), `route '${p}' is not an ADMIN_IA leaf`);
}
assert.equal(new Set(routePaths).size, routePaths.length, 'duplicate route');

// --- ONE isZipFile ------------------------------------------------------------
const src = await readFile(new URL('../src/components/admin/backupFile.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(src, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'backupFile.ts',
});
const { isZipFile } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);
assert.equal(isZipFile({ name: 'dgp-backup.zip', type: '' }), true);
assert.equal(isZipFile({ name: 'BACKUP.ZIP', type: '' }), true);
assert.equal(isZipFile({ name: 'backup', type: 'application/zip' }), true);
assert.equal(isZipFile({ name: 'backup', type: 'application/x-zip-compressed' }), true);
assert.equal(isZipFile({ name: 'iam.json', type: 'application/json' }), false);
assert.equal(isZipFile({ name: 'zip.json', type: '' }), false);

console.log('admin-page regression checks passed');
