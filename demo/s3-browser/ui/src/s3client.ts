import {
  S3Client,
  ListObjectsV2Command,
  HeadObjectCommand,
  PutObjectCommand,
  DeleteObjectCommand,
  GetObjectCommand,
  ListBucketsCommand,
  CreateBucketCommand,
  DeleteBucketCommand,
} from '@aws-sdk/client-s3';
import type { ListObjectsV2CommandOutput } from '@aws-sdk/client-s3';
import { getSignedUrl } from '@aws-sdk/s3-request-presigner';
import type { S3Object, ListResult, BucketInfo } from './types';
import { detectDefaultEndpoint } from './utils';

let activeBucket = localStorage.getItem('dg-bucket') || 'ror-builds-xdelta';
let activeRegion = localStorage.getItem('dg-region') || 'us-east-1';

export function getBucket(): string {
  return activeBucket;
}

export function setBucket(name: string) {
  activeBucket = name;
  localStorage.setItem('dg-bucket', name);
}

export function getRegion(): string {
  return activeRegion;
}

export function setRegion(region: string) {
  activeRegion = region;
  localStorage.setItem('dg-region', region);
  cachedClient = null;
}

export function getEndpoint(): string {
  return localStorage.getItem('dg-endpoint') || detectDefaultEndpoint();
}

export function setEndpoint(url: string) {
  localStorage.setItem('dg-endpoint', url);
  cachedClient = null;
}

export function getCredentials(): { accessKeyId: string; secretAccessKey: string } {
  return {
    accessKeyId: localStorage.getItem('dg-access-key-id') || '',
    secretAccessKey: localStorage.getItem('dg-secret-access-key') || '',
  };
}

export function setCredentials(accessKeyId: string, secretAccessKey: string) {
  localStorage.setItem('dg-access-key-id', accessKeyId);
  localStorage.setItem('dg-secret-access-key', secretAccessKey);
  cachedClient = null;
}

export function hasCredentials(): boolean {
  const ak = localStorage.getItem('dg-access-key-id');
  const sk = localStorage.getItem('dg-secret-access-key');
  return Boolean(ak && sk);
}

let cachedClient: S3Client | null = null;

function getClient(): S3Client {
  if (!cachedClient) {
    const creds = getCredentials();
    cachedClient = new S3Client({
      endpoint: getEndpoint(),
      region: activeRegion,
      credentials: { accessKeyId: creds.accessKeyId, secretAccessKey: creds.secretAccessKey },
      forcePathStyle: true,
    });
  }
  return cachedClient;
}

const MAX_LIST_PAGES = 10; // ~10,000 objects max

export async function listObjects(prefix = ''): Promise<ListResult> {
  const client = getClient();
  const allObjects: S3Object[] = [];
  const folderSet = new Set<string>();
  let continuationToken: string | undefined;
  let isTruncated = false;

  for (let page = 0; page < MAX_LIST_PAGES; page++) {
    const resp: ListObjectsV2CommandOutput = await client.send(
      new ListObjectsV2Command({
        Bucket: activeBucket,
        Prefix: prefix || undefined,
        Delimiter: '/',
        ContinuationToken: continuationToken,
      })
    );

    for (const cp of resp.CommonPrefixes || []) {
      if (cp.Prefix) folderSet.add(cp.Prefix);
    }

    for (const o of resp.Contents || []) {
      allObjects.push({
        key: o.Key || '',
        size: o.Size || 0,
        lastModified: o.LastModified?.toISOString() || '',
        etag: o.ETag || '',
        headers: {},
      });
    }

    if (!resp.IsTruncated) break;
    continuationToken = resp.NextContinuationToken;
    if (!continuationToken) break;

    // If we've reached the page cap, signal truncation
    if (page === MAX_LIST_PAGES - 1) {
      isTruncated = true;
    }
  }

  return { objects: allObjects, folders: Array.from(folderSet), isTruncated };
}

/** Fetch HEAD metadata for a single object (called lazily by InspectorPanel). */
export async function headObject(key: string): Promise<{
  headers: Record<string, string>;
  storageType?: string;
  storedSize?: number;
}> {
  const client = getClient();
  const headResp = await client.send(
    new HeadObjectCommand({ Bucket: activeBucket, Key: key })
  );
  const headers: Record<string, string> = {};

  if (headResp.ContentType) headers['content-type'] = headResp.ContentType;
  if (headResp.ContentLength !== undefined) headers['content-length'] = String(headResp.ContentLength);
  if (headResp.ETag) headers['etag'] = headResp.ETag;
  if (headResp.LastModified) headers['last-modified'] = headResp.LastModified.toUTCString();

  if (headResp.Metadata) {
    for (const [k, v] of Object.entries(headResp.Metadata)) {
      headers[`x-amz-meta-${k}`] = v;
    }
  }
  if (headResp.StorageClass) {
    headers['x-amz-storage-class'] = headResp.StorageClass;
  }

  const meta = headResp.Metadata || {};
  const dgNote = meta['dg-note'];
  if (dgNote) headers['x-amz-storage-type'] = dgNote;
  const dgDeltaSize = meta['dg-delta-size'];
  if (dgDeltaSize) headers['x-deltaglider-stored-size'] = dgDeltaSize;

  let storageType: string | undefined;
  let storedSize: number | undefined;
  if (headers['x-amz-storage-type']) storageType = headers['x-amz-storage-type'];
  const storedStr = headers['x-deltaglider-stored-size'];
  if (storedStr) storedSize = parseInt(storedStr, 10);

  return { headers, storageType, storedSize };
}

export async function uploadObject(key: string, data: Blob | ArrayBuffer): Promise<void> {
  const client = getClient();
  const body = data instanceof Blob ? new Uint8Array(await data.arrayBuffer()) : new Uint8Array(data);
  await client.send(
    new PutObjectCommand({
      Bucket: activeBucket,
      Key: key,
      Body: body,
      ContentType: 'application/octet-stream',
    })
  );
}

export async function deleteObject(key: string): Promise<void> {
  const client = getClient();
  await client.send(new DeleteObjectCommand({ Bucket: activeBucket, Key: key }));
}

export async function deleteObjects(keys: string[]): Promise<void> {
  await Promise.all(keys.map((key) => deleteObject(key)));
}

/** Recursively list and delete all objects under a prefix (folder). */
export async function deletePrefix(pfx: string): Promise<void> {
  const client = getClient();
  let continuationToken: string | undefined;
  for (;;) {
    const resp = await client.send(
      new ListObjectsV2Command({
        Bucket: activeBucket,
        Prefix: pfx,
        ContinuationToken: continuationToken,
      })
    );
    const keys = (resp.Contents || []).map((o) => o.Key).filter(Boolean) as string[];
    if (keys.length > 0) await deleteObjects(keys);
    if (!resp.IsTruncated) break;
    continuationToken = resp.NextContinuationToken;
    if (!continuationToken) break;
  }
}

export async function downloadObject(key: string): Promise<Blob> {
  const client = getClient();
  const resp = await client.send(new GetObjectCommand({ Bucket: activeBucket, Key: key }));
  if (!resp.Body) throw new Error('Empty response body');
  // resp.Body is a ReadableStream in the browser
  const stream = resp.Body as ReadableStream<Uint8Array>;
  const reader = stream.getReader();
  const chunks: BlobPart[] = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    if (value) chunks.push(value as unknown as BlobPart);
  }
  return new Blob(chunks);
}

export async function getPresignedUrl(key: string, expiresInSeconds = 7 * 24 * 3600 - 1): Promise<string> {
  const client = getClient();
  const command = new GetObjectCommand({ Bucket: activeBucket, Key: key });
  return getSignedUrl(client, command, { expiresIn: expiresInSeconds });
}

export function getObjectUrl(key: string): string {
  return `${getEndpoint()}/${activeBucket}/${encodeURIComponent(key)}`;
}

// ── Connection testing ──

export interface TestConnectionResult {
  ok: boolean;
  buckets?: string[];
  error?: string;
}

/** Test connectivity with arbitrary settings without affecting the active client. */
export async function testConnection(
  endpoint: string,
  accessKeyId: string,
  secretAccessKey: string,
  region = 'us-east-1',
): Promise<TestConnectionResult> {
  const client = new S3Client({
    endpoint,
    region,
    credentials: { accessKeyId, secretAccessKey },
    forcePathStyle: true,
  });
  try {
    const resp = await client.send(new ListBucketsCommand({}));
    const buckets = (resp.Buckets || []).map((b) => b.Name || '').filter(Boolean);
    return { ok: true, buckets };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

// ── Bucket operations ──

export async function listBuckets(): Promise<BucketInfo[]> {
  const client = getClient();
  const resp = await client.send(new ListBucketsCommand({}));
  return (resp.Buckets || []).map((b) => ({
    name: b.Name || '',
    creationDate: b.CreationDate?.toISOString() || '',
  }));
}

export async function createBucket(name: string): Promise<void> {
  const client = getClient();
  await client.send(new CreateBucketCommand({ Bucket: name }));
}

export async function deleteBucket(name: string): Promise<void> {
  const client = getClient();
  await client.send(new DeleteBucketCommand({ Bucket: name }));
}
