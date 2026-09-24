import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/codeTokens.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'codeTokens.ts',
});
const { splitCodeTokens } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

const codes = (s) => splitCodeTokens(s).filter((x) => x.kind === 'code').map((x) => x.text);

assert.deepEqual(codes('use the CLI flag --set-bootstrap-password to reset it'), ['--set-bootstrap-password']);
assert.deepEqual(codes('Set DGP_CACHE_MB or --config.'), ['DGP_CACHE_MB', '--config']);
assert.deepEqual(codes('a well-known name -- and a dash'), []);
assert.deepEqual(codes('x-amz--foo'), []); // inside a word: not a flag
assert.deepEqual(codes('no tokens here'), []);
// Round trip: the segments concatenate back to the input.
const s = 'Run --set-bootstrap-password (DGP_BOOTSTRAP_PASSWORD_HASH).';
assert.equal(splitCodeTokens(s).map((x) => x.text).join(''), s);
assert.deepEqual(splitCodeTokens(''), []);
// Token at the very start, and two tokens one character apart.
assert.deepEqual(codes('--config and DGP_X/DGP_Y'), ['--config', 'DGP_X', 'DGP_Y']);
assert.equal(splitCodeTokens('--a --b').map((x) => x.text).join(''), '--a --b');
// Safari < 16.4 cannot parse a regex lookbehind: none may reach the bundle
// (ESLint enforces the same rule; this keeps the module itself honest).
assert.ok(!/\(\?<[=!]/.test(source), 'codeTokens.ts must not use a regex lookbehind');

// Source guard (issue #92 item 7): no CLI flag in UI prose outside <code>.
// A line may carry a flag only inside <code>, a CodeTokenText text prop, or a
// comment; CSS custom properties ('--x': …, var(--x)) are not flags.
const offenders = [];
async function walk(dir) {
  for (const ent of await readdir(dir, { withFileTypes: true })) {
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
await walk(new URL('../src/', import.meta.url));
assert.deepEqual(offenders, [], `CLI flags in prose must render in <code> (use CodeTokenText): ${offenders.join(', ')}`);

console.log('code-tokens regression tests passed');
