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
import { getSignedUrl } from '@aws-sdk/s3-request-presigner';
import type { S3Object, ListResult, StorageStats, BucketInfo } from './types';

const DEFAULT_BUCKET = 'default';

let activeBucket = DEFAULT_BUCKET;

export function getBucket(): string {
  return activeBucket;
}

export function setBucket(name: string) {
  activeBucket = name;
}

function detectDefaultEndpoint(): string {
  if (typeof window !== 'undefined') {
    // Demo UI runs on S3 port + 1; derive the S3 endpoint automatically
    const port = parseInt(window.location.port, 10);
    if (port) {
      return `${window.location.protocol}//${window.location.hostname}:${port - 1}`;
    }
  }
  return 'http://localhost:9002';
}

export function getEndpoint(): string {
  return localStorage.getItem('dg-endpoint') || detectDefaultEndpoint();
}

export function setEndpoint(url: string) {
  localStorage.setItem('dg-endpoint', url);
}

export function getCredentials(): { accessKeyId: string; secretAccessKey: string } {
  return {
    accessKeyId: localStorage.getItem('dg-access-key-id') || 'minioadmin',
    secretAccessKey: localStorage.getItem('dg-secret-access-key') || 'minioadmin',
  };
}

export function setCredentials(accessKeyId: string, secretAccessKey: string) {
  localStorage.setItem('dg-access-key-id', accessKeyId);
  localStorage.setItem('dg-secret-access-key', secretAccessKey);
}

function makeClient(): S3Client {
  const creds = getCredentials();
  return new S3Client({
    endpoint: getEndpoint(),
    region: 'us-east-1',
    credentials: { accessKeyId: creds.accessKeyId, secretAccessKey: creds.secretAccessKey },
    forcePathStyle: true,
  });
}

export async function listObjects(prefix = ''): Promise<ListResult> {
  const client = makeClient();
  const cmd = new ListObjectsV2Command({
    Bucket: activeBucket,
    Prefix: prefix || undefined,
    Delimiter: '/',
  });
  const resp = await client.send(cmd);

  const folders = (resp.CommonPrefixes || [])
    .map((cp) => cp.Prefix || '')
    .filter(Boolean);

  const objects: S3Object[] = (resp.Contents || []).map((o) => ({
    key: o.Key || '',
    size: o.Size || 0,
    lastModified: o.LastModified?.toISOString() || '',
    etag: o.ETag || '',
    headers: {},
  }));

  // Enrich each object with HEAD to get storage + custom metadata headers
  const enriched = await Promise.all(
    objects.map(async (obj) => {
      try {
        const headResp = await client.send(
          new HeadObjectCommand({ Bucket: activeBucket, Key: obj.key })
        );
        const headers: Record<string, string> = {};
        // Pull standard metadata from the SDK response
        if (headResp.ContentType) headers['content-type'] = headResp.ContentType;
        if (headResp.ContentLength !== undefined) headers['content-length'] = String(headResp.ContentLength);
        if (headResp.ETag) headers['etag'] = headResp.ETag;
        if (headResp.LastModified) headers['last-modified'] = headResp.LastModified.toUTCString();
        // SDK exposes x-amz-meta-* headers via Metadata map (prefix stripped)
        if (headResp.Metadata) {
          for (const [k, v] of Object.entries(headResp.Metadata)) {
            headers[`x-amz-meta-${k}`] = v;
          }
        }
        if (headResp.StorageClass) {
          headers['x-amz-storage-class'] = headResp.StorageClass;
        }

        // The proxy returns custom headers x-amz-storage-type and
        // x-deltaglider-stored-size which the SDK strips. However it also
        // sends equivalent values as x-amz-meta-dg-note and
        // x-amz-meta-dg-delta-size which the SDK does expose.
        const meta = headResp.Metadata || {};
        const dgNote = meta['dg-note'];
        if (dgNote) headers['x-amz-storage-type'] = dgNote;
        const dgDeltaSize = meta['dg-delta-size'];
        if (dgDeltaSize) headers['x-deltaglider-stored-size'] = dgDeltaSize;

        obj.headers = headers;
        obj.storageType = headers['x-amz-storage-type'] || undefined;
        const storedStr = headers['x-deltaglider-stored-size'];
        if (storedStr) obj.storedSize = parseInt(storedStr, 10);
      } catch {
        // ignore HEAD failures
      }
      return obj;
    })
  );

  return { objects: enriched, folders };
}

export async function uploadObject(key: string, data: Blob | ArrayBuffer): Promise<void> {
  const client = makeClient();
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
  const client = makeClient();
  await client.send(new DeleteObjectCommand({ Bucket: activeBucket, Key: key }));
}

export async function deleteObjects(keys: string[]): Promise<void> {
  await Promise.all(keys.map((key) => deleteObject(key)));
}

export async function downloadObject(key: string): Promise<Blob> {
  const client = makeClient();
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
  const client = makeClient();
  const command = new GetObjectCommand({ Bucket: activeBucket, Key: key });
  return getSignedUrl(client, command, { expiresIn: expiresInSeconds });
}

export function getObjectUrl(key: string): string {
  return `${getEndpoint()}/${activeBucket}/${encodeURIComponent(key)}`;
}

export async function getStats(): Promise<StorageStats> {
  const resp = await fetch(`${getEndpoint()}/stats`);
  return resp.json();
}

// ── Bucket operations ──

export async function listBuckets(): Promise<BucketInfo[]> {
  const client = makeClient();
  const resp = await client.send(new ListBucketsCommand({}));
  return (resp.Buckets || []).map((b) => ({
    name: b.Name || '',
    creationDate: b.CreationDate?.toISOString() || '',
  }));
}

export async function createBucket(name: string): Promise<void> {
  const client = makeClient();
  await client.send(new CreateBucketCommand({ Bucket: name }));
}

export async function deleteBucket(name: string): Promise<void> {
  const client = makeClient();
  await client.send(new DeleteBucketCommand({ Bucket: name }));
}
