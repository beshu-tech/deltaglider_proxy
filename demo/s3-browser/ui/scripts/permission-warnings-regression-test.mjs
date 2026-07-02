import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile a TS module to an importable data: URL. `replaceImports` rewrites
// the relative './storagePath' import to a pre-built data URL (no bundler).
async function loadModule(relPath, fileName, replaceImports = {}) {
  const url = new URL(relPath, import.meta.url);
  let source = await readFile(url, 'utf8');
  for (const [spec, dataUrl] of Object.entries(replaceImports)) {
    source = source.replaceAll(`'${spec}'`, `'${dataUrl}'`);
  }
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

const storagePathUrl = await loadModule('../src/storagePath.ts', 'storagePath.ts');
const modUrl = await loadModule('../src/components/permissionWarnings.ts', 'permissionWarnings.ts', {
  '../storagePath': storagePathUrl,
});
const { unknownBucketWarnings, invalidPatternWarnings } = await import(modUrl);

const BUCKETS = ['beshu', 'debug', 'archive'];

// THE bug from the user's config: `ror/lib/*` parses to bucket `ror` which is
// not a real bucket. Must warn, and suggest nothing absurd.
{
  const w = unknownBucketWarnings(['ror/lib/*'], BUCKETS);
  assert.equal(w.length, 1);
  assert.equal(w[0].bucket, 'ror');
  assert.equal(w[0].resource, 'ror/lib/*');
}

// A correct resource on a real bucket → no warning.
assert.deepEqual(unknownBucketWarnings(['beshu/ror/libs/*'], BUCKETS), []);

// Wildcard-only and bare bucket → no warning.
assert.deepEqual(unknownBucketWarnings(['*'], BUCKETS), []);
assert.deepEqual(unknownBucketWarnings(['beshu'], BUCKETS), []);
assert.deepEqual(unknownBucketWarnings(['beshu/*'], BUCKETS), []);

// Template bucket → skipped (can't validate `${...}`).
assert.deepEqual(unknownBucketWarnings(['${iam:username}/scrap/*'], BUCKETS), []);

// Mixed list: one good, one bad → exactly one warning for the bad one.
{
  const w = unknownBucketWarnings(['beshu/ror/libs/*', 'ror/lib/*'], BUCKETS);
  assert.equal(w.length, 1);
  assert.equal(w[0].bucket, 'ror');
}

// Duplicate bad bucket across rows → de-duped to one warning.
{
  const w = unknownBucketWarnings(['ror/a/*', 'ror/b/*'], BUCKETS);
  assert.equal(w.length, 1);
}

// Near-miss suggestion: typo of a real bucket gets suggested.
{
  const w = unknownBucketWarnings(['beshuu/x/*'], BUCKETS); // 1 extra char
  assert.equal(w.length, 1);
  assert.equal(w[0].suggestion, 'beshu');
}

// Empty known-bucket list (still loading) → no false positives.
assert.deepEqual(unknownBucketWarnings(['ror/lib/*'], []), []);

// invalidPatternWarnings — mirrors the backend validate_permissions rejects.
// Valid patterns → no warnings.
assert.deepEqual(invalidPatternWarnings(['beshu/ror/libs/*']), []);
assert.deepEqual(invalidPatternWarnings(['beshu', 'beshu/*', '*']), []);
assert.deepEqual(invalidPatternWarnings(['beshu/my-bucket.name/*']), []); // hyphens/dots OK
assert.deepEqual(invalidPatternWarnings(['${iam:username}/x/*']), []);     // template OK here
// Mid-pattern `*` → rejected (only trailing allowed).
{
  const w = invalidPatternWarnings(['beshu/*/thing']);
  assert.equal(w.length, 1);
  assert.ok(w[0].includes('mid-pattern'));
}
// Internal whitespace → rejected.
{
  const w = invalidPatternWarnings(['beshu/a b/*']);
  assert.equal(w.length, 1);
  assert.ok(w[0].includes('space or control'));
}
// A wildcard inside the bucket segment → rejected as mid-pattern.
assert.equal(invalidPatternWarnings(['b*cket/x']).length, 1);
// Trailing whitespace alone is trimmed away → valid (no false positive).
assert.deepEqual(invalidPatternWarnings(['beshu/ok/*  ']), []);
// Multiple bad patterns → one message each, de-duped.
{
  const w = invalidPatternWarnings(['a/*/b', 'a/*/b', 'c d/*']);
  assert.equal(w.length, 2);
}

console.log('permission warnings regression checks passed');
