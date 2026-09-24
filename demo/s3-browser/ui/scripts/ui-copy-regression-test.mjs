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
 */
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';

const ROOT = process.env.UI_COPY_ROOT ?? new URL('../src/', import.meta.url).pathname;

async function files(dir) {
  const out = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...(await files(p)));
    else if (/\.tsx?$/.test(e.name)) out.push(p);
  }
  return out;
}

function stripComments(src) {
  const blank = (m) => m.replace(/[^\n]/g, ' ');
  return src.replace(/\/\*[\s\S]*?\*\//g, blank).replace(/(^|[^:\\])\/\/[^\n]*/g, (m, p) => p + blank(m.slice(p.length)));
}

const RULES = [
  { name: 'raw backtick in a quoted string', re: /\w="[^"\n{}`]*`[^"\n{}]*"/ },
  { name: 'internal API wording', re: /\bsection API\b|\bon (GET|PUT)\b|\b(HEAD|GET|PUT|LIST|API) calls\b|\bsome calls\b/i },
  { name: 'request-rule vocabulary', re: /operator-authored|\bsynthesi[sz]ed\b|\badmission (block|chain)s?\b|(?<!request )\brule tester\b/i },
];

const violations = [];
for (const f of await files(ROOT)) {
  if (f.includes('/schemas/') || f.endsWith('docsBundle.ts')) continue;
  const lines = stripComments(await readFile(f, 'utf8')).split('\n');
  lines.forEach((line, i) => {
    for (const r of RULES) {
      if (r.re.test(line)) violations.push(`${f.replace(ROOT, 'src/')}:${i + 1} ${r.name}: ${line.trim().slice(0, 120)}`);
    }
  });
}
assert.deepEqual(violations, [], `UI copy violations:\n${violations.join('\n')}`);
console.log('ui-copy regression checks passed');
