import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'vitest';
import { isZipFile } from '../components/admin/backupFile';

test('every ADMIN_IA leaf renders a page', async () => {
  // adminNavigation.tsx and adminRoutes.tsx carry JSX (icons, panels), so read
  // the path literals from source instead of importing them. A leaf with no
  // route silently fell through to the dashboard; a route with no leaf is dead.
  const iaSrc = await readFile(new URL('../components/adminNavigation.tsx', import.meta.url), 'utf8');
  const iaPaths = [...iaSrc.matchAll(/^\s+path: '([^']+)',$/gm)].map((m) => m[1]);
  assert.ok(iaPaths.length >= 17, `expected the admin IA leaves, got ${iaPaths.length}`);

  const routesSrc = await readFile(new URL('../components/admin/adminRoutes.tsx', import.meta.url), 'utf8');
  const table = routesSrc.slice(routesSrc.indexOf('const ADMIN_ROUTES'));
  const routePaths = [...table.matchAll(/^ {2}'([^']+)': \{$/gm)].map((m) => m[1]);

  for (const p of iaPaths) assert.ok(routePaths.includes(p), `ADMIN_IA leaf '${p}' has no route`);
  // `setup` is reachable by URL and the palette, not the sidebar.
  for (const p of routePaths) {
    assert.ok(p === 'setup' || iaPaths.includes(p), `route '${p}' is not an ADMIN_IA leaf`);
  }
  assert.equal(new Set(routePaths).size, routePaths.length, 'duplicate route');
});

test('ONE isZipFile', () => {
  assert.equal(isZipFile({ name: 'dgp-backup.zip', type: '' }), true);
  assert.equal(isZipFile({ name: 'BACKUP.ZIP', type: '' }), true);
  assert.equal(isZipFile({ name: 'backup', type: 'application/zip' }), true);
  assert.equal(isZipFile({ name: 'backup', type: 'application/x-zip-compressed' }), true);
  assert.equal(isZipFile({ name: 'iam.json', type: 'application/json' }), false);
  assert.equal(isZipFile({ name: 'zip.json', type: '' }), false);
});

test('URL-synced modals open only through their open* helper', async () => {
  // The YAML and IAM modals close whenever the URL lacks ?modal=…. A setter
  // call with a mode (not null) outside the open* helper skips the URL push,
  // so the modal closes at once: the palette's Show YAML did nothing.
  const src = await readFile(new URL('../components/AdminPage.tsx', import.meta.url), 'utf8');
  const opens = [...src.matchAll(/set(YamlModalMode|IamYamlMode)\((?!null\))/g)];
  assert.equal(opens.length, 2, `expected only openYamlModal/openIamYamlModal to set a mode, got ${opens.length}`);
});
