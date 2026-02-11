import { S3Client, ListObjectsV2Command } from '@aws-sdk/client-s3';
import type { S3Object, ListResult, StorageStats } from './types';

const BUCKET = 'data';

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

function makeClient(): S3Client {
  return new S3Client({
    endpoint: getEndpoint(),
    region: 'us-east-1',
    credentials: { accessKeyId: 'minioadmin', secretAccessKey: 'minioadmin' },
    forcePathStyle: true,
  });
}

export async function listObjects(prefix = ''): Promise<ListResult> {
  const client = makeClient();
  const cmd = new ListObjectsV2Command({
    Bucket: BUCKET,
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
        const headResp = await fetch(
          `${getEndpoint()}/${BUCKET}/${obj.key}`,
          { method: 'HEAD' }
        );
        const headers: Record<string, string> = {};
        headResp.headers.forEach((value, key) => {
          headers[key] = value;
        });
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
  const body = data instanceof Blob ? await data.arrayBuffer() : data;
  await fetch(`${getEndpoint()}/${BUCKET}/${key}`, {
    method: 'PUT',
    body,
    headers: { 'Content-Type': 'application/octet-stream' },
  });
}

export async function deleteObject(key: string): Promise<void> {
  await fetch(`${getEndpoint()}/${BUCKET}/${key}`, { method: 'DELETE' });
}

export async function deleteObjects(keys: string[]): Promise<void> {
  await Promise.all(keys.map((key) => deleteObject(key)));
}

export async function downloadObject(key: string): Promise<Blob> {
  const resp = await fetch(`${getEndpoint()}/${BUCKET}/${key}`);
  return resp.blob();
}

export async function getStats(): Promise<StorageStats> {
  const resp = await fetch(`${getEndpoint()}/stats`);
  return resp.json();
}
