/**
 * #120, conversion marker: the pure words and states of a bucket's
 * conversion, and the source guard that no UI path writes a read policy
 * (`plaintext_reads: serve` or `refuse`) any more. Removing the
 * deprecated line (`plaintext_reads: null`, "Convert now") is the one
 * write that names the field.
 */
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { test } from 'vitest';
import type { ConversionView } from '../adminApi';
import { conversionCountText, conversionRowState, versionedWarnings } from '../conversionView';

function view(over: Partial<ConversionView> = {}): ConversionView {
  return {
    open: true,
    run: 'ab'.repeat(16),
    adopted_after_cutoff: 0,
    job_on_this_node: null,
    serves_plaintext_deprecated: false,
    ...over,
  };
}

test('the count sentence: one bucket, several, one object, a capped count, none', () => {
  assert.equal(
    conversionCountText([{ bucket: 'releases', objects: 1204, more: false, versioned: false }]),
    'The 1,204 objects already in bucket releases are not encrypted yet. After you apply, a re-encrypt job encrypts them in the background. They stay readable while the job runs.',
  );
  assert.match(
    conversionCountText([{ bucket: 'releases', objects: 1, more: false, versioned: false }])!,
    /^The 1 object already in bucket releases is not encrypted yet\./,
  );
  assert.match(
    conversionCountText([
      { bucket: 'releases', objects: 10000, more: true, versioned: false },
      { bucket: 'downloads', objects: 2, more: false, versioned: false },
    ])!,
    /^More than 10,002 objects already in 2 buckets \(releases, downloads\) are not encrypted yet\./,
  );
  assert.equal(conversionCountText([]), null);
  assert.equal(conversionCountText(undefined), null);
});

test('only a versioned bucket gets the versioning warning', () => {
  const w = versionedWarnings([
    { bucket: 'releases', objects: 1, more: false, versioned: true },
    { bucket: 'downloads', objects: 1, more: false, versioned: false },
  ]);
  assert.equal(w.length, 1);
  assert.match(w[0], /bucket releases.*noncurrent versions/);
});

test('the row state of every conversion', () => {
  assert.equal(conversionRowState(undefined), null);
  assert.equal(conversionRowState(view({ run: undefined, serves_plaintext_deprecated: true })), null);
  for (const status of ['queued', 'running', 'cancelling']) {
    assert.deepEqual(conversionRowState(view({ job_on_this_node: { id: 4, status, percent: 30 } })), {
      kind: 'running',
      jobId: 4,
      percent: 30,
    });
  }
  assert.deepEqual(conversionRowState(view()), { kind: 'resume', jobId: null, failed: false });
  assert.deepEqual(conversionRowState(view({ job_on_this_node: { id: 4, status: 'completed_with_errors', percent: 100 } })), {
    kind: 'resume',
    jobId: 4,
    failed: true,
  });
  // The deadline passed: stopped, whatever the job says.
  assert.deepEqual(conversionRowState(view({ open: false, job_on_this_node: { id: 4, status: 'running', percent: 5 } })), {
    kind: 'stopped',
    jobId: 4,
  });
});

const SRC = join(__dirname, '..');

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return e.name === '__tests__' || e.name === 'test' ? [] : sourceFiles(p);
    return /\.(ts|tsx)$/.test(e.name) && !/\.test\.tsx?$/.test(e.name) ? [p] : [];
  });
}

test('no UI path writes a read policy (plaintext_reads: serve or refuse)', () => {
  const bad: string[] = [];
  for (const file of sourceFiles(SRC)) {
    const code = readFileSync(file, 'utf8');
    for (const m of code.matchAll(/plaintext_reads['"]?\s*[:=]\s*['"](serve|refuse)['"]/g)) {
      bad.push(`${relative(SRC, file)}: ${m[0]}`);
    }
  }
  assert.deepEqual(bad, [], 'the server converts the history; only Convert now names the field, with null');
});
