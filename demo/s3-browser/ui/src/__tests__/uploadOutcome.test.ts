/** The upload outcome model (UX harvest D23–D31): the truth table. */
import { expect, test } from 'vitest';
import { canRetry, mergeStaged, outcomeHeadline, queueOutcome, stagedHeadline, stagedRows, uploadButtonLabel } from '../uploadOutcome';

const f = (name: string, size: number, lastModified = 1) => new File(['x'.repeat(size)], name, { lastModified });

test('the same file twice is one entry; another file with a staged path replaces it in place', () => {
  const a = f('a.txt', 8);
  const b = f('b.txt', 1);
  expect(mergeStaged([a, b], [a]).files).toEqual([a, b]);
  const a2 = f('a.txt', 3, 2);
  const merged = mergeStaged([a, b], [a2]);
  expect(merged.files).toEqual([a2, b]);
  expect(merged.replaced).toEqual(['a.txt']);
});

test('counts and size are those of the files that can go', () => {
  const ok = f('ok.bin', 10);
  const big = f('big.bin', 200);
  expect(stagedHeadline(stagedRows([ok], []))).toBe('1 file ready to upload · 10 B');
  const rows = stagedRows([ok, big], [{ file: big, reason: 'too big' }]);
  expect(rows.map((r) => r.state)).toEqual(['staged', 'refused']);
  expect(stagedHeadline(rows)).toBe('2 files staged · 1 can be uploaded (10 B) · 1 refused');
  expect(stagedHeadline(stagedRows([big], [{ file: big, reason: 'too big' }]))).toBe('1 file staged · none can be uploaded');
});

test('the button names the destination only when it is valid', () => {
  expect(uploadButtonLabel(31, null)).toBe('Upload 31 files');
  expect(uploadButtonLabel(1, 'builds/')).toBe('Upload 1 file to builds/');
});

test('the banner is success only when every file succeeded', () => {
  expect(queueOutcome([]).tone).toBe('idle');
  expect(queueOutcome([{ status: 'success' }, { status: 'uploading' }]).tone).toBe('running');
  expect(queueOutcome([{ status: 'success' }, { status: 'success' }]).tone).toBe('success');
  const partial = queueOutcome([{ status: 'success' }, { status: 'error', retry: 'after-fix' }, { status: 'error', retry: 'retry' }]);
  expect(partial).toMatchObject({ tone: 'partial', done: 1, failed: 2, retryable: 1 });
  expect(outcomeHeadline(partial, 'qa-quota/').title).toBe('1 of 3 files uploaded');
  expect(queueOutcome([{ status: 'error', retry: 'never' }]).tone).toBe('failed');
  expect(queueOutcome([{ status: 'cancelled' }]).tone).toBe('cancelled');
});

test('Retry only where it can help', () => {
  expect(canRetry({ status: 'error', retry: 'retry' })).toBe(true);
  expect(canRetry({ status: 'error', retry: 'after-fix' })).toBe(false);
  expect(canRetry({ status: 'error', retry: 'never' })).toBe(false);
  expect(canRetry({ status: 'cancelled' })).toBe(true);
});
