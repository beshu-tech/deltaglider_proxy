import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile a TS module to an importable data: URL (no bundler). permissionConditions.ts
// has no relative imports, so the dependency graph is trivial.
async function loadModule(relPath, fileName) {
  const url = new URL(relPath, import.meta.url);
  const source = await readFile(url, 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ES2020,
      target: ts.ScriptTarget.ES2020,
      importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
    },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const modUrl = await loadModule('../src/components/permissionConditions.ts', 'permissionConditions.ts');
const {
  getConditionValue,
  setConditionValue,
  getConditionArray,
  setConditionArray,
  hasConditions,
} = await import(modUrl);

const OP = 'IpAddress';
const KEY = 'aws:SourceIp';
const set = (conds, value) => setConditionValue(conds, OP, KEY, value);
const get = (conds) => getConditionValue(conds, OP, KEY);

// (1) THE REGRESSION: trailing comma must NOT persist an empty-string element.
{
  const result = set(undefined, '192.168.0.0/16, 10.0.0.0/8, ');
  assert.deepEqual(result, { IpAddress: { 'aws:SourceIp': ['192.168.0.0/16', '10.0.0.0/8'] } });
  const arr = result.IpAddress['aws:SourceIp'];
  assert.deepEqual(arr, ['192.168.0.0/16', '10.0.0.0/8']);
  assert.ok(!arr.includes(''), 'array must not contain an empty string');
}

// (2) trailing comma with a single survivor coalesces to a scalar string,
//     and crucially never ['192.168.0.0/16', ''].
{
  const result = set(undefined, '192.168.0.0/16, ');
  assert.deepEqual(result, { IpAddress: { 'aws:SourceIp': '192.168.0.0/16' } });
  assert.equal(typeof result.IpAddress['aws:SourceIp'], 'string');
  assert.notDeepEqual(result.IpAddress['aws:SourceIp'], ['192.168.0.0/16', '']);
}

// (3) all-empty input removes the key entirely.
{
  const result = set(undefined, ' , , ');
  assert.deepEqual(result, {});
}

// (3b) all-empty input removes a previously-set key (and prunes the op block).
{
  const seeded = set(undefined, 'a/16, b/8');
  const result = set(seeded, ' , , ');
  assert.deepEqual(result, {});
}

// (4) clean multi-value passes through unchanged.
{
  const result = set(undefined, 'a/16, b/8');
  assert.deepEqual(result, { IpAddress: { 'aws:SourceIp': ['a/16', 'b/8'] } });
}

// (5) round-trip via getConditionValue: no dangling ', '.
{
  const roundTripped = get(set(undefined, 'a, b, '));
  assert.equal(roundTripped, 'a, b');
}

// (6) hasConditions returns false for the all-empty result.
{
  const result = set(undefined, ' , , ');
  assert.equal(hasConditions(result), false);
}

// (6b) hasConditions true once a real value lands.
{
  assert.equal(hasConditions(set(undefined, 'a/16, b/8')), true);
}

// ── Array contract for s3:prefix: "" (root) MUST be preserved (Obstacle 1) ──
const PREFIX_OP = 'StringLike';
const PREFIX_KEY = 's3:prefix';
const setArr = (conds, vals) => setConditionArray(conds, PREFIX_OP, PREFIX_KEY, vals);
const getArr = (conds) => getConditionArray(conds, PREFIX_OP, PREFIX_KEY);

// (A) The empty string "" (list bucket root) survives — the whole point.
{
  const result = setArr(undefined, ['', 'ror/libs/', 'ror/libs/*']);
  assert.deepEqual(result.StringLike['s3:prefix'], ['', 'ror/libs/', 'ror/libs/*']);
  assert.ok(result.StringLike['s3:prefix'].includes(''), 'root "" must be preserved');
}

// (B) A whitespace-only entry IS dropped, but a real "" is NOT.
{
  const result = setArr(undefined, ['', '  ', 'ror/']);
  assert.deepEqual(result.StringLike['s3:prefix'], ['', 'ror/']);
}

// (C) De-dupe, order-preserving.
{
  const result = setArr(undefined, ['ror/', 'ror/', 'a/']);
  assert.deepEqual(result.StringLike['s3:prefix'], ['ror/', 'a/']);
}

// (D) All-empty (no real entries, no root) removes the key entirely.
{
  assert.deepEqual(setArr(undefined, ['  ', '']), { StringLike: { 's3:prefix': [''] } }); // root kept
  assert.deepEqual(setArr(undefined, ['  ', ' ']), {}); // no root, only noise → removed
  assert.deepEqual(setArr(undefined, []), {});
}

// (E) Round-trip through getConditionArray is lossless for root + entries.
{
  const conds = setArr(undefined, ['', 'ror/libs/']);
  assert.deepEqual(getArr(conds), ['', 'ror/libs/']);
}

// (F) getConditionArray reads a legacy scalar (incl. "") as a single-entry list.
{
  assert.deepEqual(getConditionArray({ StringLike: { 's3:prefix': 'ror/' } }, PREFIX_OP, PREFIX_KEY), ['ror/']);
  assert.deepEqual(getConditionArray({ StringLike: { 's3:prefix': '' } }, PREFIX_OP, PREFIX_KEY), ['']);
  assert.deepEqual(getConditionArray(undefined, PREFIX_OP, PREFIX_KEY), []);
}

console.log('permission conditions regression checks passed');
