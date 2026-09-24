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

const errorHandlingUrl = await load('errorHandling.ts');
const { zipPreflightError, ZIP_MAX_BYTES, downloadZip } = await import(
  await load('zipDownload.ts', { './errorHandling': errorHandlingUrl, './utils': await load('utils.ts') }),
);
const { ApiError, isSessionExpired } = await import(errorHandlingUrl);

// The limits mirror the server constants in src/api/admin/objects.rs.
const ZIP_MAX_KEYS = 10_000;
const ZIP_MAX_URL_LENGTH = 60_000;
const src = await readFile(new URL('../src/zipDownload.ts', import.meta.url), 'utf8');
assert.match(src, /const ZIP_MAX_KEYS = 10_000;/);
assert.match(src, /const ZIP_MAX_URL_LENGTH = 60_000;/);
const rust = await readFile(new URL('../../../../src/api/admin/objects.rs', import.meta.url), 'utf8');
assert.match(rust, new RegExp(`const MAX_BULK_OBJECTS: usize = ${ZIP_MAX_KEYS.toLocaleString('en-US').replaceAll(',', '_')};`));
assert.equal(ZIP_MAX_BYTES, 500 * 1024 * 1024);
assert.match(rust, /const MAX_ZIP_BYTES: u64 = 500 \* 1024 \* 1024;/);

assert.equal(zipPreflightError(1, 100), null);
assert.equal(zipPreflightError(ZIP_MAX_KEYS, ZIP_MAX_URL_LENGTH), null);
assert.match(zipPreflightError(0, 50), /no files/);
assert.match(zipPreflightError(ZIP_MAX_KEYS + 1, 100), /10,001 files.*at most 10,000/);
assert.match(zipPreflightError(900, ZIP_MAX_URL_LENGTH + 1), /too many files \(900\)/);

// --- downloadZip, non-2xx: the picked file is deleted, the error explains ---
function fakes(status, body = '') {
  const calls = { removed: 0, wrote: 0 };
  const handle = {
    createWritable: async () => { calls.wrote++; return new WritableStream(); },
    remove: async () => { calls.removed++; },
  };
  const deps = {
    picker: async () => handle,
    fetch: async () => new Response(status === 200 ? 'PK' : body, {
      status,
      headers: { 'content-type': 'application/json' },
    }),
  };
  return { calls, deps };
}

{
  const { calls, deps } = fakes(413);
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), /more than 500\.0 MB, the limit for one ZIP/);
  assert.equal(calls.removed, 1, '413: the empty file is deleted');
  assert.equal(calls.wrote, 0);
}
{
  const { calls, deps } = fakes(404, JSON.stringify({ error: 'None of the selected files could be read.' }));
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), (e) => {
    assert.ok(e instanceof ApiError);
    assert.equal(e.status, 404);
    assert.match(e.message, /None of the selected files could be read/);
    return true;
  });
  assert.equal(calls.removed, 1, '404: the empty file is deleted');
}
{
  const { calls, deps } = fakes(401, JSON.stringify({ error: 'unauthorized' }));
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), (e) => isSessionExpired(e));
  assert.equal(calls.removed, 1);
}
{
  // Network failure: the file is deleted and the error passes through.
  const { calls, deps } = fakes(200);
  deps.fetch = async () => { throw new TypeError('Failed to fetch'); };
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), /Failed to fetch/);
  assert.equal(calls.removed, 1);
}
{
  const { calls, deps } = fakes(200);
  assert.equal(await downloadZip('/zip', 'a.zip', deps), 'saved');
  assert.equal(calls.removed, 0, 'success keeps the file');
  assert.equal(calls.wrote, 1);
}
{
  // A browser without remove() still gets the error.
  const { deps } = fakes(502);
  const h = await deps.picker();
  delete h.remove;
  deps.picker = async () => h;
  await assert.rejects(downloadZip('/zip', 'a.zip', deps), /ZIP download failed \(502\)/);
}

console.log('zip-download regression checks passed');
