/**
 * Operator-facing copy rules (issue #92 items 10 and 20), checked over every
 * component source with comments stripped:
 *   - no raw backticks inside a quoted string (they render as literal `…`;
 *     use <code> for a real identifier),
 *   - no internal API wording ("section API", "on PUT", "HEAD calls" —
 *     HTTP traffic is "requests"),
 *   - one vocabulary for request rules: never "admission block/chain",
 *     "operator-authored", "synthesised", or "Rule tester".
 * YAML keys and identifiers are unaffected: the patterns need spaces or
 * word boundaries that identifiers do not have.
 *
 * src/__tests__ is excluded from the scan: this file's own RULES definitions
 * quote the banned phrases and would otherwise flag themselves.
 */
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import { test } from 'vitest';

const ROOT = process.env.UI_COPY_ROOT ?? new URL('../', import.meta.url).pathname;

async function files(dir: string): Promise<string[]> {
  const out: string[] = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    if (e.isDirectory() && e.name === '__tests__') continue;
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...(await files(p)));
    else if (/\.tsx?$/.test(e.name)) out.push(p);
  }
  return out;
}

function stripComments(src: string): string {
  const blank = (m: string) => m.replace(/[^\n]/g, ' ');
  return src.replace(/\/\*[\s\S]*?\*\//g, blank).replace(/(^|[^:\\])\/\/[^\n]*/g, (m, p: string) => p + blank(m.slice(p.length)));
}

/**
 * True when the line has a bare "rule tester" not immediately preceded by
 * "request " ("request rule tester" is the approved vocabulary). Written
 * without a regex lookbehind: ESLint's `no-restricted-syntax` bans lookbehind
 * literals repo-wide (Safari < 16.4 cannot parse them), so this walks matches
 * and inspects the preceding 8 characters by hand instead.
 */
function hasBareRuleTester(line: string): boolean {
  const re = /rule tester/gi;
  let m: RegExpExecArray | null;
  while ((m = re.exec(line))) {
    const before = line.slice(Math.max(0, m.index - 8), m.index).toLowerCase();
    if (before !== 'request ') return true;
  }
  return false;
}

const RULES: { name: string; test: (line: string) => boolean }[] = [
  { name: 'raw backtick in a quoted string', test: (l) => /\w="[^"\n{}`]*`[^"\n{}]*"/.test(l) },
  {
    name: 'internal API wording',
    test: (l) => /\bsection API\b|\bon (GET|PUT)\b|\b(HEAD|GET|PUT|LIST|API) calls\b|\bsome calls\b/i.test(l),
  },
  {
    name: 'request-rule vocabulary',
    test: (l) => /operator-authored|\bsynthesi[sz]ed\b|\badmission (block|chain)s?\b/i.test(l) || hasBareRuleTester(l),
  },
];

test('no UI copy violations in src/**/*.tsx', async () => {
  const violations: string[] = [];
  for (const f of await files(ROOT)) {
    if (f.includes('/schemas/') || f.endsWith('docsBundle.ts')) continue;
    const lines = stripComments(await readFile(f, 'utf8')).split('\n');
    lines.forEach((line, i) => {
      for (const r of RULES) {
        if (r.test(line)) violations.push(`${f.replace(ROOT, 'src/')}:${i + 1} ${r.name}: ${line.trim().slice(0, 120)}`);
      }
    });
  }
  assert.deepEqual(violations, [], `UI copy violations:\n${violations.join('\n')}`);
});

test('the product docs use the UI vocabulary for request rules too', async () => {
  // The YAML key `admission.blocks` stays; prose says "rule". The changelog is history.
  const DOCS = new URL('../../../../../docs/product/', import.meta.url).pathname;
  async function mdFiles(dir: string): Promise<string[]> {
    const out: string[] = [];
    for (const e of await readdir(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      if (e.isDirectory()) out.push(...(await mdFiles(p)));
      else if (e.name.endsWith('.md') && e.name !== 'changelog.md') out.push(p);
    }
    return out;
  }
  const DOC_VOCAB = /operator-authored|\badmission blocks?\b|\bsynthesi[sz]ed (public-prefix |admission )?blocks?\b|\bmatched block\b/i;
  const docViolations: string[] = [];
  for (const f of await mdFiles(DOCS)) {
    (await readFile(f, 'utf8')).split('\n').forEach((line, i) => {
      if (DOC_VOCAB.test(line) || hasBareRuleTester(line)) {
        docViolations.push(`${f.replace(DOCS, 'docs/product/')}:${i + 1}: ${line.trim().slice(0, 120)}`);
      }
    });
  }
  assert.deepEqual(docViolations, [], `Docs vocabulary violations:\n${docViolations.join('\n')}`);
});
