/**
 * The upload queue reads how the proxy stored each file from the write
 * response (P8.4): no HEAD per uploaded file, no LIST of the folder to guess
 * whether the upload created its baseline. The real s3client runs through
 * mockFetch (the AWS SDK's browser build): an unrouted request fails.
 */
import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { initFromSession } from '../s3client';
import useUploadQueue from '../useUploadQueue';

let http: ReturnType<typeof mockFetch>;
/** The write-fact headers the "proxy" answers with, by key. */
let facts: Record<string, Record<string, string>>;

// jsdom's Blob has no stream(), which lib-storage reads a browser File with.
const blobProto = Blob.prototype as { stream?: (this: Blob) => ReadableStream<Uint8Array> };
if (!blobProto.stream) {
  blobProto.stream = function stream(this: Blob) {
    return new ReadableStream<Uint8Array>({
      start: async (controller) => {
        controller.enqueue(new Uint8Array(await this.arrayBuffer()));
        controller.close();
      },
    });
  };
}

const xml = (body: string) => new Response(body, { status: 200, headers: { 'content-type': 'application/xml' } });

beforeEach(async () => {
  http = mockFetch();
  facts = {};
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://proxy.test', region: 'us-east-1', access_key_id: 'ak', secret_access_key: 'sk', bucket: 'releases',
  }));
  const keyOf = (path: string) => decodeURIComponent(path.split('?')[0].replace(/^\/releases\//, ''));
  // PutObject, and UploadPart (?partNumber=).
  http.on('PUT', /^\/releases\//, (req) => () => {
    const part = req.path.includes('partNumber=');
    return new Response('', { status: 200, headers: { etag: '"e1"', ...(part ? {} : facts[keyOf(req.path)]) } });
  });
  // CreateMultipartUpload (?uploads) and CompleteMultipartUpload (?uploadId=).
  http.on('POST', /^\/releases\//, (req) => () => {
    const key = keyOf(req.path);
    if (req.path.includes('uploads')) {
      return xml(`<InitiateMultipartUploadResult><Bucket>releases</Bucket><Key>${key}</Key><UploadId>u1</UploadId></InitiateMultipartUploadResult>`);
    }
    return new Response(
      `<CompleteMultipartUploadResult><Bucket>releases</Bucket><Key>${key}</Key><ETag>"m-2"</ETag></CompleteMultipartUploadResult>`,
      { status: 200, headers: { 'content-type': 'application/xml', ...facts[key] } },
    );
  });
  await initFromSession();
});
afterEach(() => vi.unstubAllGlobals());

const reads = () => http.calls.filter((c) => c.method === 'HEAD' || (c.method === 'GET' && c.path.startsWith('/releases')));

test('three uploads into a new folder: the facts come from the PUT responses, 0 HEADs, 0 LISTs', async () => {
  facts['f/base.zip'] = {
    'x-amz-storage-type': 'delta', 'x-deltaglider-stored-size': '40',
    'x-deltaglider-ref-sha256': 'r1', 'x-deltaglider-baseline': 'created',
  };
  facts['f/v1.zip'] = { 'x-amz-storage-type': 'delta', 'x-deltaglider-stored-size': '60', 'x-deltaglider-ref-sha256': 'r1' };
  facts['f/a.txt'] = { 'x-amz-storage-type': 'passthrough', 'x-deltaglider-stored-size': '5' };
  const { result } = renderHook(() => useUploadQueue('f'));
  act(() => result.current.addFiles([
    new File(['x'.repeat(1000)], 'base.zip'),
    new File(['y'.repeat(1000)], 'v1.zip'),
    new File(['hello'], 'a.txt'),
  ]));
  await waitFor(() => expect(result.current.stats.uploaded).toBe(3), { timeout: 10_000 });
  await waitFor(() => expect(result.current.stats.storedSize).not.toBeNull());
  // 40 + 60 + 5, plus the baseline that base.zip created (its 1000 bytes).
  expect(result.current.stats.storedSize).toBe(1105);
  expect(result.current.stats.baselineCount).toBe(1);
  expect(reads()).toEqual([]);
});

test('a multipart upload reads the facts of CompleteMultipartUpload', async () => {
  facts['f/big.zip'] = {
    'x-amz-storage-type': 'delta', 'x-deltaglider-stored-size': '77',
    'x-deltaglider-ref-sha256': 'r2', 'x-deltaglider-baseline': 'created',
  };
  const big = new File([new Uint8Array(17 * 1024 * 1024)], 'big.zip');
  const { result } = renderHook(() => useUploadQueue('f'));
  act(() => result.current.addFiles([big]));
  await waitFor(() => expect(result.current.stats.uploaded).toBe(1), { timeout: 10_000 });
  await waitFor(() => expect(result.current.stats.storedSize).toBe(77 + big.size));
  expect(result.current.stats.baselineCount).toBe(1);
  expect(http.calls.some((c) => c.method === 'POST' && c.path.includes('uploadId='))).toBe(true);
  expect(reads()).toEqual([]);
});

test('a write response without facts leaves the stored size unknown', async () => {
  const { result } = renderHook(() => useUploadQueue('f'));
  act(() => result.current.addFiles([new File(['z'], 'odd.bin')]));
  await waitFor(() => expect(result.current.stats.uploaded).toBe(1), { timeout: 10_000 });
  await act(async () => {
    await new Promise((r) => setTimeout(r, 100));
  });
  expect(result.current.stats.storedSize).toBeNull();
  expect(reads()).toEqual([]);
});
