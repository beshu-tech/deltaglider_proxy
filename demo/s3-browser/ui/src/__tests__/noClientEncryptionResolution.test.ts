/**
 * Issue #116 P2: a bucket's encryption is the server's answer
 * (`effective_encryption` on GET /api/admin/buckets, from
 * `Config::encryption_for_listed_bucket`). The UI had three resolutions of
 * its own (`resolveBackendFor`, `originBackendOf`, and a route → origin →
 * default chain in BucketsPanel), and each one ignored the bucket's own
 * `encryption` block. This scans the UI source so that no copy comes back:
 *
 *   * the two deleted resolvers by name;
 *   * a bucket's backend that falls back to the default backend
 *     (`backend_name ?? default…`, `backend || … defaultBackend`): the
 *     server already resolves `backend_name`;
 *   * a backend looked up from the list and its `encryption` read in one
 *     expression (`backends.find(…).encryption`): the shape of the old
 *     post-apply proposal, which took a backend's mode for its buckets.
 */
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { test } from 'vitest';

const SRC = join(__dirname, '..');

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return e.name === '__tests__' || e.name === 'test' ? [] : sourceFiles(p);
    return /\.(ts|tsx)$/.test(e.name) && !/\.test\.tsx?$/.test(e.name) ? [p] : [];
  });
}

const RULES: Array<{ name: string; re: RegExp; allowed?: string[] }> = [
  { name: 'resolveBackendFor', re: /\bresolveBackendFor\b/ },
  { name: 'originBackendOf', re: /\boriginBackendOf\b/ },
  { name: 'backend_name falls back to a default', re: /backend_name\s*(\?\?|\|\|)\s*\(?\s*default/i },
  {
    name: 'route → origin → default chain',
    re: /\.backend\s*\|\|[\s\S]{0,120}?\|\|\s*defaultBackend/,
  },
  {
    name: 'encryption read off a backend looked up in the list',
    re: /backends\??\.find\([^\n]*?\)\s*\??\.\s*encryption/,
  },
];

test('the UI never resolves a bucket encryption itself', () => {
  const bad: string[] = [];
  for (const file of sourceFiles(SRC)) {
    const rel = relative(SRC, file);
    const text = readFileSync(file, 'utf8');
    for (const rule of RULES) {
      if (rule.allowed?.includes(rel)) continue;
      if (rule.re.test(text)) bad.push(`${rel}: ${rule.name}`);
    }
  }
  assert.deepEqual(bad, [], 'show `effective_encryption` from GET /api/admin/buckets instead');
});

test('the guard sees the shapes it refuses', () => {
  const samples: Record<string, string> = {
    resolveBackendFor: 'const info = resolveBackendFor(eff.backend, backends, d);',
    originBackendOf: 'originBackendOf(o, defaultBackend)',
    'backend_name falls back to a default': 'const key = o.backend_name ?? defaultBackend ?? "";',
    'route → origin → default chain':
      "rowByName.get(b)?.backend ||\n              originBackendByName.get(b) ||\n              defaultBackend ||\n              'default'",
    'encryption read off a backend looked up in the list':
      'backends.find((b) => b.name === backendName)?.encryption?.mode',
  };
  for (const rule of RULES) assert.ok(rule.re.test(samples[rule.name]), rule.name);
});
