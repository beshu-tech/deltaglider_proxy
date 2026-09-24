import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile bucketPolicyLookup.ts (type-only imports) to an importable data: URL.
const source = await readFile(new URL('../src/bucketPolicyLookup.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'bucketPolicyLookup.ts',
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
const { bucketPolicyFor } = await import(moduleUrl);

const exact = { compression: false };
const lower = { compression: true };
const config = { bucket_policies: { Releases: exact, downloads: lower } };

assert.equal(bucketPolicyFor(config, 'Releases'), exact, 'exact key wins');
assert.equal(bucketPolicyFor(config, 'Downloads'), lower, 'falls back to the lower-cased key');
assert.equal(bucketPolicyFor(config, 'downloads'), lower);
assert.equal(bucketPolicyFor(config, 'db-archive'), undefined, 'unknown bucket → undefined');
assert.equal(bucketPolicyFor(null, 'releases'), undefined, 'no config → undefined');
assert.equal(bucketPolicyFor(undefined, 'releases'), undefined);
assert.equal(bucketPolicyFor({}, 'releases'), undefined, 'no bucket_policies → undefined');

console.log('bucket-policy-lookup regression checks passed');
