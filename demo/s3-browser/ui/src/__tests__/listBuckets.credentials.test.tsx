/**
 * F61 (epic #146): the 5 s listBuckets cache ignored who is signed in. After a
 * logout and a sign-in as another user within five seconds, the second user
 * got the first user's bucket list. The cache is keyed by the credentials and
 * the endpoint, and dropped whenever they change.
 */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { json, mockFetch } from '../test/fetchMock';
import { disconnect, initFromSession, listBuckets, setCredentials, setEndpoint } from '../s3client';

let http: ReturnType<typeof mockFetch>;
let signedInAs = 'ak-dana';

beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/session/s3-credentials', () =>
    json({ endpoint: 'http://proxy.test', region: 'us-east-1', access_key_id: signedInAs, secret_access_key: 'sk', bucket: '' }),
  );
  http.on('PUT', '/_/api/admin/session/s3-credentials', json({ ok: true }));
  http.on('POST', '/_/api/admin/session/s3-credentials', json({ ok: true }));
  http.on('GET', '/_/api/admin/buckets', json({ buckets: [] }));
  // The proxy lists the buckets of the signed-in user only.
  http.on('GET', /^\/(\?|$)/, (req) => {
    const who = /Credential=([^/]+)\//.exec(req.headers['authorization'] ?? '')?.[1] ?? 'unknown';
    return new Response(
      `<ListAllMyBucketsResult><Buckets><Bucket><Name>buckets-of-${who}</Name><CreationDate>2026-01-01T00:00:00.000Z</CreationDate></Bucket></Buckets></ListAllMyBucketsResult>`,
      { status: 200, headers: { 'content-type': 'application/xml' } },
    );
  });
});
afterEach(() => {
  disconnect();
  vi.unstubAllGlobals();
});

test('a user who signs in right after another never sees the first user\'s buckets', async () => {
  signedInAs = 'ak-dana';
  await initFromSession();
  expect((await listBuckets({ includeOrigins: false })).map((b) => b.name)).toEqual(['buckets-of-ak-dana']);
  disconnect();
  await setCredentials('ak-bob', 'sk-bob');
  expect((await listBuckets({ includeOrigins: false })).map((b) => b.name)).toEqual(['buckets-of-ak-bob']);
});

test('the same user within the window shares one listing', async () => {
  signedInAs = 'ak-dana';
  await initFromSession();
  await Promise.all([listBuckets({ includeOrigins: false }), listBuckets({ includeOrigins: false })]);
  expect(http.calls.filter((c) => c.method === 'GET' && /^\/(\?|$)/.test(c.path))).toHaveLength(1);
});

test('a new endpoint is a new listing', async () => {
  signedInAs = 'ak-dana';
  await initFromSession();
  await listBuckets({ includeOrigins: false });
  setEndpoint('http://other.test');
  await listBuckets({ includeOrigins: false });
  expect(http.calls.filter((c) => c.method === 'GET' && /^\/(\?|$)/.test(c.path))).toHaveLength(2);
});

test('only dropClient forgets the client: every credential change drops the listing cache with it', () => {
  const text = readFileSync(join(__dirname, '..', 's3client.ts'), 'utf8').replace(/\/\/.*$/gm, '');
  const resets = text.match(/cachedClient = null/g) ?? [];
  // The declaration `let cachedClient: S3Client | null = null;` does not match; the one reset is in dropClient.
  expect(resets).toHaveLength(1);
  expect(text).toMatch(/function dropClient\(\)[^}]*cachedClient = null[^}]*listBucketsShared = null/s);
});
