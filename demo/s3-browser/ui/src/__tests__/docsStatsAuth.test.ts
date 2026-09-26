/**
 * Explore finding 19: the docs called `/_/stats` exempt from auth and showed
 * `curl .../_/stats` with no session, but the route needs an admin session
 * (401 otherwise; `tests/status_endpoints_test.rs` pins it). This scans the
 * product docs so the claim cannot come back.
 */
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'vitest';

const DOCS = join(__dirname, '../../../../../docs/product');

function markdownFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
    e.isDirectory() ? markdownFiles(join(dir, e.name)) : e.name.endsWith('.md') ? [join(dir, e.name)] : [],
  );
}

test('no doc sends /_/stats without a session, or calls it unauthenticated', () => {
  const bad: string[] = [];
  for (const file of markdownFiles(DOCS)) {
    if (file.endsWith('changelog.md')) continue;
    readFileSync(file, 'utf8').split('\n').forEach((line, i) => {
      const where = `${file.slice(DOCS.length + 1)}:${i + 1}`;
      if (/curl\b[^\n]*\/_\/stats/.test(line) && !/\s-b\s/.test(line)) bad.push(`${where}: curl without -b`);
      if (/\/_\/stats/.test(line) && /exempt|without credentials|unauthenticated|no auth/i.test(line)) {
        bad.push(`${where}: claims no auth`);
      }
    });
  }
  assert.deepEqual(bad, []);
});
