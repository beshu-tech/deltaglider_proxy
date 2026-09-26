/** src/codeTokens.ts */
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { test } from 'vitest';
import { splitCodeTokens } from '../codeTokens';

const sourceUrl = new URL('../codeTokens.ts', import.meta.url);

const codes = (s: string) => splitCodeTokens(s).filter((x) => x.kind === 'code').map((x) => x.text);

test('splitCodeTokens: recognized code tokens', () => {
  assert.deepEqual(codes('use the CLI flag --set-bootstrap-password to reset it'), ['--set-bootstrap-password']);
  assert.deepEqual(codes('Set DGP_CACHE_MB or --config.'), ['DGP_CACHE_MB', '--config']);
  assert.deepEqual(codes('a well-known name -- and a dash'), []);
  assert.deepEqual(codes('x-amz--foo'), []); // inside a word: not a flag
  assert.deepEqual(codes('no tokens here'), []);
});

test('splitCodeTokens: round trip and edge cases', () => {
  // Round trip: the segments concatenate back to the input.
  const s = 'Run --set-bootstrap-password (DGP_BOOTSTRAP_PASSWORD_HASH).';
  assert.equal(splitCodeTokens(s).map((x) => x.text).join(''), s);
  assert.deepEqual(splitCodeTokens(''), []);
  // Token at the very start, and two tokens one character apart.
  assert.deepEqual(codes('--config and DGP_X/DGP_Y'), ['--config', 'DGP_X', 'DGP_Y']);
  assert.equal(splitCodeTokens('--a --b').map((x) => x.text).join(''), '--a --b');
});

test('splitCodeTokens: no regex lookbehind in source (Safari < 16.4)', async () => {
  // Safari < 16.4 cannot parse a regex lookbehind: none may reach the bundle
  // (ESLint enforces the same rule; this keeps the module itself honest).
  const source = await readFile(sourceUrl, 'utf8');
  assert.ok(!/\(\?<[=!]/.test(source), 'codeTokens.ts must not use a regex lookbehind');
});

test('source guard: no CLI flag in UI prose outside <code> (issue #92 item 7)', async () => {
  // A line may carry a flag only inside <code>, a CodeTokenText text prop, or a
  // comment; CSS custom properties ('--x': …, var(--x)) are not flags.
  // src/__tests__ is excluded: this file's own fixtures/patterns would trip it.
  const offenders: string[] = [];
  async function walk(dir: URL): Promise<void> {
    for (const ent of await readdir(dir, { withFileTypes: true })) {
      if (ent.isDirectory() && ent.name === '__tests__') continue;
      const p = new URL(ent.name + (ent.isDirectory() ? '/' : ''), dir);
      if (ent.isDirectory()) await walk(p);
      else if (ent.name.endsWith('.tsx')) {
        const lines = (await readFile(p, 'utf8')).split('\n');
        lines.forEach((line, i) => {
          if (/^\s*(\/\/|\*|\/\*)/.test(line)) return;
          const stripped = line
            .replace(/var\(--[\w-]+/g, '')
            .replace(/['"]--[\w-]+['"]\s*(as never\])?\s*\]?:/g, '')
            .replace(/<code[^>]*>[^<]*<\/code>/g, '');
          if (/(^|[\s'"`(])--[a-z][a-z0-9-]+/.test(stripped) && !/CodeTokenText/.test(line)) {
            offenders.push(`${p.pathname.split('/src/')[1]}:${i + 1}`);
          }
        });
      }
    }
  }
  await walk(new URL('../', import.meta.url));
  assert.deepEqual(offenders, [], `CLI flags in prose must render in <code> (use CodeTokenText): ${offenders.join(', ')}`);
});
