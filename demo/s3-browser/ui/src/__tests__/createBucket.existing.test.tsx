/**
 * UX harvest C54: S3 CreateBucket answers 200 for a bucket the caller
 * already owns, so "releases" (exists) was reported as created. The
 * client asks the bucket list first and refuses an existing name.
 */
import { afterEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { createBucket, initFromSession } from '../s3client';
import { ApiError } from '../errorHandling';

afterEach(() => vi.unstubAllGlobals());

function setup() {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://proxy.test', region: 'us-east-1', access_key_id: 'ak', secret_access_key: 'sk', bucket: 'releases',
  }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  http.on('GET', /^\/(\?|$)/, () => new Response(
    '<ListAllMyBucketsResult><Buckets><Bucket><Name>releases</Name><CreationDate>2026-01-01T00:00:00.000Z</CreationDate></Bucket></Buckets></ListAllMyBucketsResult>',
    { status: 200, headers: { 'content-type': 'application/xml' } },
  ));
  // The proxy answers 200 for an owned bucket, as S3 does in us-east-1.
  http.on('PUT', /^\/[a-z0-9.-]+\/?(\?|$)/, () => new Response('', { status: 200 }));
  return http;
}

test('an existing name is refused before any CreateBucket is sent', async () => {
  const http = setup();
  await initFromSession();
  const err = await createBucket('releases').catch((e: unknown) => e);
  expect(err).toBeInstanceOf(ApiError);
  expect((err as ApiError).status).toBe(409);
  expect((err as ApiError).message).toBe('A bucket named releases already exists.');
  expect(http.calls.filter((c) => c.method === 'PUT')).toHaveLength(0);
});

test('a new name is created', async () => {
  const http = setup();
  await initFromSession();
  await createBucket('downloads');
  expect(http.calls.filter((c) => c.method === 'PUT')).toHaveLength(1);
});
