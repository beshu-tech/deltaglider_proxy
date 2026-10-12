/**
 * listBuckets reads the bucket origins through the shared react-query entry
 * (P8.8b): a component's useBucketOrigins right after it sends no second
 * GET /buckets.
 */
import { afterEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { initFromSession, listBuckets } from '../s3client';
import { queryClient } from '../queries/client';
import { qk } from '../queries/keys';
import { getBucketOrigins } from '../adminApi';

afterEach(() => vi.unstubAllGlobals());

test('listBuckets fills the origins query that the panels read', async () => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/session/s3-credentials', json({
    endpoint: 'http://proxy.test', region: 'us-east-1', access_key_id: 'ak', secret_access_key: 'sk', bucket: 'releases',
  }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [{ name: 'releases', backend_name: 'hetzner-fsn1' }] }));
  http.on('GET', /^\/(\?|$)/, () => new Response(
    '<ListAllMyBucketsResult><Buckets><Bucket><Name>releases</Name><CreationDate>2026-01-01T00:00:00.000Z</CreationDate></Bucket></Buckets></ListAllMyBucketsResult>',
    { status: 200, headers: { 'content-type': 'application/xml' } },
  ));
  await initFromSession();
  const buckets = await listBuckets();
  expect(buckets[0].backend?.backendName).toBe('hetzner-fsn1');
  await queryClient.fetchQuery({ queryKey: qk.backends.origins(), queryFn: getBucketOrigins });
  expect(http.callsTo('GET', '/_/api/admin/buckets')).toHaveLength(1);
});
