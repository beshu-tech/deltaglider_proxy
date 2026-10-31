/**
 * #120, conversion marker: the pure words and states of a bucket's
 * conversion, and the source guard that no UI source names the removed
 * read-policy field: the server refuses a config that has it.
 */
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { test } from 'vitest';
import type { ConversionView } from '../adminApi';
import {
  CONVERSION_POLL_MS,
  adoptedText,
  conversionCountText,
  conversionPhrase,
  conversionPollMs,
  conversionRowState,
  deadlineText,
  sentence,
  versionedWarnings,
  withoutVersionedNotes,
} from '../conversionView';

function view(over: Partial<ConversionView> = {}): ConversionView {
  return {
    open: true,
    run: 'ab'.repeat(16),
    adopted_after_cutoff: 0,
    job_on_this_node: null,
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
  assert.equal(
    w[0],
    'The backend keeps old versions of bucket releases. The job encrypts the current objects; their old unencrypted versions stay on the backend.',
  );
});

test("the dialog drops the server's versioning note of a versioned bucket only", () => {
  const server = "bucket 'releases' keeps old versions on its backend (versioning is enabled or suspended): … (the header X-DGP-Purge-Plaintext-Versions: true …)";
  const other = "bucket 'downloads' keeps old versions on its backend …";
  const previews = [
    { bucket: 'releases', objects: 1, more: false, versioned: true },
    { bucket: 'downloads', objects: 1, more: false, versioned: false },
  ];
  assert.deepEqual(withoutVersionedNotes([server, other, 'x'], previews), [other, 'x']);
  assert.deepEqual(withoutVersionedNotes(undefined, previews), []);
});

test('objects stored after encryption started: one, many', () => {
  assert.equal(adoptedText(1), '1 object was stored after encryption started; it was encrypted too');
  assert.equal(adoptedText(1204), '1,204 objects were stored after encryption started; they were encrypted too');
});

test('the row state of every conversion', () => {
  assert.equal(conversionRowState(undefined), null);
  assert.equal(conversionRowState(view({ run: undefined })), null);
  for (const status of ['queued', 'running', 'cancelling']) {
    assert.deepEqual(conversionRowState(view({ job_on_this_node: { id: 4, status, percent: 30 } })), {
      kind: 'running',
      jobId: 4,
      percent: 30,
    });
  }
  assert.deepEqual(conversionRowState(view()), { kind: 'resume', jobId: null, ended: 'none', deadline: null });
  assert.deepEqual(conversionRowState(view({ deadline: 9, job_on_this_node: { id: 4, status: 'completed_with_errors', percent: 100 } })), {
    kind: 'resume',
    jobId: 4,
    ended: 'failed',
    deadline: 9,
  });
  // Cancelled is not a failure (review 2026-10-31, finding 6).
  assert.deepEqual(conversionRowState(view({ job_on_this_node: { id: 4, status: 'cancelled', percent: 40 } })), {
    kind: 'resume',
    jobId: 4,
    ended: 'cancelled',
    deadline: null,
  });
  // The deadline passed: stopped, whatever the job says.
  assert.deepEqual(conversionRowState(view({ open: false, job_on_this_node: { id: 4, status: 'running', percent: 5 } })), {
    kind: 'stopped',
    jobId: 4,
  });
});

test('every state reads as a sentence-case sentence on its own', () => {
  const states = [
    conversionRowState(view({ job_on_this_node: { id: 1, status: 'running', percent: 42 } }))!,
    conversionRowState(view({ job_on_this_node: { id: 1, status: 'queued', percent: null } }))!,
    conversionRowState(view({ deadline: DEADLINE }))!,
    conversionRowState(view({ deadline: DEADLINE, job_on_this_node: { id: 1, status: 'failed', percent: null } }))!,
    conversionRowState(view({ deadline: DEADLINE, job_on_this_node: { id: 1, status: 'cancelled', percent: null } }))!,
    conversionRowState(view())!,
    conversionRowState(view({ open: false }))!,
  ];
  const lines = states.map((s) => sentence(conversionPhrase(s)));
  const until = deadlineText(DEADLINE);
  assert.deepEqual(lines, [
    'Encrypting the existing objects, 42%.',
    'Starting to encrypt the existing objects…',
    `Some existing objects are not encrypted yet, and this proxy runs no re-encrypt job for them; unencrypted objects stay readable until ${until}.`,
    `The re-encrypt job stopped with failures; unencrypted objects stay readable until ${until}.`,
    `The re-encrypt job was cancelled; unencrypted objects stay readable until ${until}.`,
    'Some existing objects are not encrypted yet, and this proxy runs no re-encrypt job for them; unencrypted objects stay readable.',
    'The conversion stopped before it finished; objects that are not encrypted yet fail to read.',
  ]);
  // No jargon (review 2026-10-31, finding 7).
  for (const l of lines) assert.doesNotMatch(l, /window|cutoff|instance/);
});

const DEADLINE = 1_793_604_800;

/** Review 2026-10-31, finding 5: 3 s only while a job runs; a conversion
 *  that waits for Resume refetches once, just after its deadline. */
test('the polling rule of the bucket list', () => {
  const row = (conversion?: ConversionView) => ({ effective_encryption: { conversion } });
  const now = (DEADLINE - 600) * 1000; // 10 minutes before the deadline
  const running = view({ deadline: DEADLINE, job_on_this_node: { id: 1, status: 'running', percent: 3 } });
  assert.equal(conversionPollMs(undefined, now), false);
  assert.equal(conversionPollMs([row()], now), false);
  assert.equal(conversionPollMs([row(view({ run: undefined }))], now), false);
  assert.equal(conversionPollMs([row(view({ open: false }))], now), false, 'stopped changes only on a click');
  assert.equal(conversionPollMs([row(view())], now), false, 'resume with no deadline known: never');
  assert.equal(conversionPollMs([row(view({ deadline: DEADLINE }))], now), 601_000, 'resume: once, after the deadline');
  assert.equal(conversionPollMs([row(view({ deadline: DEADLINE })), row(running)], now), CONVERSION_POLL_MS, 'a running job wins');
  assert.equal(conversionPollMs([row(view({ deadline: DEADLINE + 60 })), row(view({ deadline: DEADLINE }))], now), 601_000, 'earliest deadline');
  assert.equal(conversionPollMs([row(view({ deadline: DEADLINE }))], DEADLINE * 1000), CONVERSION_POLL_MS, 'never below 3 s');
  assert.equal(conversionPollMs([row(view({ deadline: DEADLINE }))], (DEADLINE + 5) * 1000), 60_000, 'past here, open on the backend: every minute');
  assert.equal(conversionPollMs([row(view({ deadline: DEADLINE + 30 * 86_400 }))], now), 86_400_000, 'at most a day (setInterval range)');
});

const SRC = join(__dirname, '..');

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return e.name === '__tests__' || e.name === 'test' ? [] : sourceFiles(p);
    return /\.(ts|tsx)$/.test(e.name) && !/\.test\.tsx?$/.test(e.name) ? [p] : [];
  });
}

test('no UI source names the removed read-policy field', () => {
  // Built at runtime, so that this file does not match itself.
  const removed = ['plaintext', 'reads'].join('_');
  const bad: string[] = [];
  for (const file of sourceFiles(SRC)) {
    if (readFileSync(file, 'utf8').includes(removed)) bad.push(relative(SRC, file));
  }
  assert.deepEqual(bad, [], 'the server refuses a config with the field; the conversion window replaces it');
});
