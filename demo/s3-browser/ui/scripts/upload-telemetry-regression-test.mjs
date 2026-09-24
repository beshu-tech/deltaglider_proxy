import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile a TS module to an importable data: URL. `replaceImports` rewrites
// bare relative imports to already-built data URLs so the dependency graph
// (uploadTelemetry -> utils) resolves without a bundler.
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

const utilsUrl = await loadModule('../src/utils.ts', 'utils.ts');
const moduleUrl = await loadModule('../src/uploadTelemetry.ts', 'uploadTelemetry.ts', {
  './utils': utilsUrl,
});
const {
  appendThroughputSample,
  clampPercent,
  estimateCompletedParts,
  estimateInFlightParts,
  estimateTotalParts,
  mergeTotalBytes,
  movingAverageSpeedBps,
  uploadDisplayPath,
  uploadRetryAdvice,
} = await import(moduleUrl);

// Issue #92 comment item 4: "Retry" after a failure that repeats cannot
// succeed; transient failures offer it. The code decides before the status.
const A = uploadRetryAdvice;
assert.equal(A(403, 'AccessDenied', 'Access Denied'), 'never', 'permission rejection');
assert.equal(A(403, undefined), 'never');
assert.equal(A(413, 'EntityTooLarge'), 'never');
assert.equal(A(404, 'NoSuchBucket'), 'never');
assert.equal(A(400, 'InvalidArgument'), 'never');
assert.equal(A(undefined, 'NoSuchBucket'), 'never');
assert.equal(A(400, undefined), 'never', 'status fallback');
// Transient S3 codes sent with status 400 / 404.
assert.equal(A(400, 'RequestTimeout'), 'retry');
assert.equal(A(400, 'IncompleteBody'), 'retry');
assert.equal(A(400, 'BadDigest'), 'retry');
assert.equal(A(404, 'NoSuchUpload'), 'retry', 'multipart state is per node; a retry starts a new upload');
// Quota: retry once space is freed, with the reason visible.
assert.equal(A(403, 'AccessDenied', 'Bucket quota exceeded: 24 MB used + 3 MB upload > 25 MB limit'), 'after-fix');
assert.equal(A(403, 'AccessDenied', 'Bucket is frozen (quota = 0)'), 'after-fix');
assert.equal(A(403, 'QuotaExceeded'), 'after-fix');
// No HTTP answer, throttling, server errors.
assert.equal(A(undefined, undefined), 'retry', 'network error: no HTTP answer');
assert.equal(A(0, undefined), 'retry', 'network error: status 0');
assert.equal(A(500, 'InternalError'), 'retry');
assert.equal(A(502, undefined), 'retry');
assert.equal(A(503, 'SlowDown'), 'retry');
assert.equal(A(429, undefined), 'retry');
assert.equal(A(408, 'RequestTimeout'), 'retry');

// Folder uploads show the path under the destination, not just the file name.
assert.equal(uploadDisplayPath('dest/rel/sub1/README.md', 'dest'), 'rel/sub1/README.md');
assert.equal(uploadDisplayPath('rel/sub2/README.md', ''), 'rel/sub2/README.md');
assert.equal(uploadDisplayPath('a/b/app.tar', 'a/b'), 'app.tar');
assert.equal(uploadDisplayPath('destination-other/x', 'dest'), 'destination-other/x');

assert.equal(clampPercent(-10), 0);
assert.equal(clampPercent(140), 100);
assert.equal(clampPercent(42.5), 42.5);

// A legitimate 0-byte telemetry total must NOT fall back to the item size
// (the `||` bug). undefined/null still fall back; a real positive wins.
assert.equal(mergeTotalBytes(0, 1024), 0);
assert.equal(mergeTotalBytes(undefined, 1024), 1024);
assert.equal(mergeTotalBytes(2048, 1024), 2048);
assert.equal(mergeTotalBytes(null, 1024), 1024);

const partSize = 16 * 1024 * 1024;
const totalBytes = 50 * 1024 * 1024;
assert.equal(estimateTotalParts(totalBytes, partSize), 4);
assert.equal(estimateCompletedParts(0, totalBytes, partSize), 0);
assert.equal(estimateCompletedParts(16 * 1024 * 1024, totalBytes, partSize), 1);
assert.equal(estimateCompletedParts(totalBytes, totalBytes, partSize), 4);

assert.equal(estimateInFlightParts('queued', 4, 1, 4), 0);
assert.equal(estimateInFlightParts('uploading', 4, 0, 4), 4);
assert.equal(estimateInFlightParts('uploading', 4, 3, 4), 1);
assert.equal(estimateInFlightParts('completing', 4, 4, 4), 0);

const samples = [];
const withA = appendThroughputSample(samples, { atMs: 1000, loadedBytes: 0 }, 5000);
const withB = appendThroughputSample(withA, { atMs: 3000, loadedBytes: 4000 }, 5000);
const withC = appendThroughputSample(withB, { atMs: 7000, loadedBytes: 12000 }, 5000);
assert.equal(withC.length, 2);
assert.equal(Math.round(movingAverageSpeedBps(withC)), 2000);

console.log('upload telemetry regression checks passed');
