import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile a JSX-free TS module to an importable data: URL. The admin
// nav tree-walk lives in its own .ts module precisely so it can be
// imported here without an icon/JSX factory.
async function loadModule(relPath, fileName) {
  const url = new URL(relPath, import.meta.url);
  const source = await readFile(url, 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ES2020,
      target: ts.ScriptTarget.ES2020,
    },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const treeUrl = await loadModule('../src/adminNavTree.ts', 'adminNavTree.ts');
const { findEntry, dirtyDotForEntry } = await import(treeUrl);

// A plain-data stand-in for ADMIN_IA: same shape (groups → entries, flat —
// the 7-group IA has NO children arrays), no JSX. The strings mirror the
// real IA so the test documents the invariants a reader expects.
const IA = [
  {
    group: 'Overview',
    entries: [{ path: 'dashboard', label: 'Dashboard' }],
  },
  {
    group: 'Access',
    entries: [
      { path: 'access/credentials', label: 'Credentials & mode', dirtyKeys: ['access/credentials'] },
      { path: 'access/users', label: 'Users' },
      { path: 'access/admission', label: 'Request rules', dirtyKeys: ['admission'] },
    ],
  },
  {
    group: 'Jobs',
    entries: [
      { path: 'jobs', label: 'Jobs', dirtyKeys: ['jobs/replication', 'jobs/lifecycle'] },
    ],
  },
  {
    group: 'System',
    entries: [
      {
        path: 'system',
        label: 'System',
        dirtyKeys: ['system/listener', 'system/caches', 'system/logging', 'system/sync'],
      },
    ],
  },
];

// ── findEntry ────────────────────────────────────────────────────────────────
assert.equal(findEntry(IA, 'dashboard')?.label, 'Dashboard');
assert.equal(findEntry(IA, 'access/users')?.label, 'Users');
assert.equal(findEntry(IA, 'jobs')?.label, 'Jobs');
assert.equal(findEntry(IA, 'nope'), undefined);
assert.equal(findEntry(IA, 'access'), undefined, 'group prefixes are not entries');

// ── dirtyDotForEntry: multi-key leaves ───────────────────────────────────────
const jobs = findEntry(IA, 'jobs');
const system = findEntry(IA, 'system');
const credentials = findEntry(IA, 'access/credentials');
const users = findEntry(IA, 'access/users');
const admission = findEntry(IA, 'access/admission');

// ANY of a leaf's keys lights it.
assert.equal(dirtyDotForEntry(jobs, new Set(['jobs/lifecycle'])), true, 'one of two keys lights Jobs');
assert.equal(dirtyDotForEntry(jobs, new Set(['jobs/replication'])), true);
assert.equal(dirtyDotForEntry(jobs, new Set(['jobs/replication', 'jobs/lifecycle'])), true);
assert.equal(dirtyDotForEntry(system, new Set(['system/caches'])), true, 'one card lights System');

// Keys never bleed across leaves.
assert.equal(dirtyDotForEntry(jobs, new Set(['system/caches'])), false);
assert.equal(dirtyDotForEntry(system, new Set(['jobs/lifecycle'])), false);
assert.equal(dirtyDotForEntry(credentials, new Set(['jobs/lifecycle'])), false);

// Admission's dirty key is the section name (its panel's historical default).
assert.equal(dirtyDotForEntry(admission, new Set(['admission'])), true);

// Keyless (immediate-save) leaves never light, whatever is dirty.
assert.equal(dirtyDotForEntry(users, new Set(['admission', 'jobs/replication'])), false);

// Empty dirty set → nothing lights anywhere.
for (const leaf of [jobs, system, credentials, users, admission]) {
  assert.equal(dirtyDotForEntry(leaf, new Set()), false);
}

// Children roll-up still works (kept generic even though the live IA is flat).
const parent = { path: 'p', children: [{ path: 'p/a', dirtyKeys: ['k'] }, { path: 'p/b' }] };
assert.equal(dirtyDotForEntry(parent, new Set(['k'])), true, 'parent rolls up');
assert.equal(dirtyDotForEntry(parent.children[1], new Set(['k'])), false, 'sibling stays off');

console.log('admin nav tree regression checks passed');
