import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Regression guard for issue #92 comment item 6: a bulk ZIP the server would
// refuse (too many keys, a request line above 64 KiB) used to start an anchor
// download that silently failed. The preflight now names the reason first.

async function load(file, rewrite = {}) {
  const source = await readFile(new URL(`../src/${file}`, import.meta.url), 'utf8');
  let { outputText } = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
    fileName: file,
  });
  for (const [from, to] of Object.entries(rewrite)) {
    outputText = outputText.replaceAll(`from '${from}'`, `from '${to}'`);
  }
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const { zipPreflightError, ZIP_MAX_KEYS, ZIP_MAX_URL_LENGTH, ZIP_MAX_BYTES } = await import(
  await load('zipDownload.ts', { './errorHandling': await load('errorHandling.ts') }),
);

// The limits mirror the server constants in src/api/admin/objects.rs.
const rust = await readFile(new URL('../../../../src/api/admin/objects.rs', import.meta.url), 'utf8');
assert.match(rust, new RegExp(`const MAX_BULK_OBJECTS: usize = ${ZIP_MAX_KEYS.toLocaleString('en-US').replaceAll(',', '_')};`));
assert.equal(ZIP_MAX_BYTES, 500 * 1024 * 1024);
assert.match(rust, /const MAX_ZIP_BYTES: u64 = 500 \* 1024 \* 1024;/);

assert.equal(zipPreflightError(1, 100), null);
assert.equal(zipPreflightError(ZIP_MAX_KEYS, ZIP_MAX_URL_LENGTH), null);
assert.match(zipPreflightError(0, 50), /no files/);
assert.match(zipPreflightError(ZIP_MAX_KEYS + 1, 100), /10,001 files.*at most 10,000/);
assert.match(zipPreflightError(900, ZIP_MAX_URL_LENGTH + 1), /too many files \(900\)/);

console.log('zip-download regression checks passed');
