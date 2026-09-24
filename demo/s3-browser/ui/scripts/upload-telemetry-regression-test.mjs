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
  isRetryableUploadFailure,
  mergeTotalBytes,
  movingAverageSpeedBps,
  uploadDisplayPath,
} = await import(moduleUrl);

// Issue #92 comment item 4: "Retry" after a 403 quota rejection cannot
// succeed; only transient failures offer it.
assert.equal(isRetryableUploadFailure(403, 'AccessDenied'), false, 'quota / permission rejection');
assert.equal(isRetryableUploadFailure(403, undefined), false);
assert.equal(isRetryableUploadFailure(413, 'EntityTooLarge'), false);
assert.equal(isRetryableUploadFailure(404, 'NoSuchBucket'), false);
assert.equal(isRetryableUploadFailure(400, 'InvalidArgument'), false);
assert.equal(isRetryableUploadFailure(undefined, 'NoSuchBucket'), false);
assert.equal(isRetryableUploadFailure(undefined, undefined), true, 'network error: no HTTP answer');
assert.equal(isRetryableUploadFailure(500, 'InternalError'), true);
assert.equal(isRetryableUploadFailure(502, undefined), true);
assert.equal(isRetryableUploadFailure(503, 'SlowDown'), true);
assert.equal(isRetryableUploadFailure(429, undefined), true);
assert.equal(isRetryableUploadFailure(408, 'RequestTimeout'), true);

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
