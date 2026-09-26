import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  appendThroughputSample,
  clampPercent,
  estimateCompletedParts,
  estimateInFlightParts,
  estimateTotalParts,
  mergeTotalBytes,
  movingAverageSpeedBps,
  uploadDisplayPath,
  uploadRetryAdvice,
  startAfterProbe,
  folderEmptyFromListing,
} from '../uploadTelemetry';

test('uploadRetryAdvice: never / retry / after-fix classification', () => {
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
});

test('startAfterProbe: cancel while the folder probe runs never starts the upload', async () => {
  {
    const ac = new AbortController();
    let started = 0;
    let release!: (v: boolean) => void;
    const probe = new Promise<boolean>((r) => {
      release = r;
    });
    const run = startAfterProbe(probe, ac.signal, async () => {
      started++;
      return 'done';
    });
    ac.abort();
    release(true);
    await assert.rejects(run, (e: unknown) => e instanceof Error && e.name === 'AbortError');
    assert.equal(started, 0, 'aborted during the probe: no upload');
  }
  {
    const ac = new AbortController();
    ac.abort();
    let started = 0;
    await assert.rejects(
      startAfterProbe(Promise.resolve(true), ac.signal, async () => {
        started++;
      }),
      (e: unknown) => e instanceof Error && e.name === 'AbortError',
    );
    assert.equal(started, 0, 'aborted before: no upload');
  }
  {
    const ac = new AbortController();
    assert.equal(
      await startAfterProbe(Promise.reject(new Error('list failed')), ac.signal, async () => 'done'),
      'done',
      'a failed probe does not block the upload',
    );
  }
});

test('folderEmptyFromListing: a baseline is per directory', () => {
  // A baseline is per directory: only objects directly in the folder count.
  assert.equal(folderEmptyFromListing({ objects: 0, truncated: false }), true, 'only subfolders, all listed');
  assert.equal(folderEmptyFromListing({ objects: 2, truncated: false }), false);
  assert.equal(folderEmptyFromListing({ objects: 1, truncated: true }), false);
  assert.equal(folderEmptyFromListing({ objects: 0, truncated: true }), undefined, 'page full of subfolders: not known');
});

test('uploadDisplayPath: folder uploads show the path under the destination', () => {
  assert.equal(uploadDisplayPath('dest/rel/sub1/README.md', 'dest'), 'rel/sub1/README.md');
  assert.equal(uploadDisplayPath('rel/sub2/README.md', ''), 'rel/sub2/README.md');
  assert.equal(uploadDisplayPath('a/b/app.tar', 'a/b'), 'app.tar');
  assert.equal(uploadDisplayPath('destination-other/x', 'dest'), 'destination-other/x');
});

test('clampPercent', () => {
  assert.equal(clampPercent(-10), 0);
  assert.equal(clampPercent(140), 100);
  assert.equal(clampPercent(42.5), 42.5);
});

test('mergeTotalBytes: a legitimate 0-byte total must NOT fall back to the item size', () => {
  // A legitimate 0-byte telemetry total must NOT fall back to the item size
  // (the `||` bug). undefined/null still fall back; a real positive wins.
  assert.equal(mergeTotalBytes(0, 1024), 0);
  assert.equal(mergeTotalBytes(undefined, 1024), 1024);
  assert.equal(mergeTotalBytes(2048, 1024), 2048);
  assert.equal(mergeTotalBytes(null, 1024), 1024);
});

test('part-count and in-flight estimation', () => {
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
});

test('throughput sampling: windowed pruning and moving average', () => {
  const samples: ReturnType<typeof appendThroughputSample> = [];
  const withA = appendThroughputSample(samples, { atMs: 1000, loadedBytes: 0 }, 5000);
  const withB = appendThroughputSample(withA, { atMs: 3000, loadedBytes: 4000 }, 5000);
  const withC = appendThroughputSample(withB, { atMs: 7000, loadedBytes: 12000 }, 5000);
  assert.equal(withC.length, 2);
  assert.equal(Math.round(movingAverageSpeedBps(withC)), 2000);
});
