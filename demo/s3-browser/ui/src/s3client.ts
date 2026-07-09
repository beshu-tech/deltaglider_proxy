import {
  S3Client,
  ListObjectsV2Command,
  type ListObjectsV2CommandOutput,
  HeadObjectCommand,
  type HeadObjectCommandOutput,
  DeleteObjectCommand,
  GetObjectCommand,
  type GetObjectCommandOutput,
  ListBucketsCommand,
  type ListBucketsCommandOutput,
  CreateBucketCommand,
  DeleteBucketCommand,
  ListMultipartUploadsCommand,
  type ListMultipartUploadsCommandOutput,
  AbortMultipartUploadCommand,
} from '@aws-sdk/client-s3';
import { Upload } from '@aws-sdk/lib-storage';
import { getSignedUrl } from '@aws-sdk/s3-request-presigner';
import type { S3Object, ListResult, BucketInfo } from './types';
import { detectDefaultEndpoint, numericCompare } from './utils';
import { createBucketOnBackend, getBucketOrigins } from './adminApi';
import { normalizeS3Error } from './errorHandling';
import {
  appendThroughputSample,
  clampPercent,
  DEFAULT_UPLOAD_PART_SIZE,
  DEFAULT_UPLOAD_QUEUE_SIZE,
  estimateCompletedParts,
  estimateInFlightParts,
  estimateTotalParts,
  movingAverageSpeedBps,
  type ThroughputSample,
  type UploadStatus,
} from './uploadTelemetry';
import {
  fetchSessionCredentials,
  storeSessionCredentials,
  clearSessionCredentials,
} from './sessionApi';

// ── In-memory credential store (never persisted to localStorage) ──

let activeBucket = localStorage.getItem('dg-bucket') || '';
let activeRegion = 'us-east-1';
let activeEndpoint = '';
let activeAccessKeyId = '';
let activeSecretAccessKey = '';

/**
 * Initialise credentials from the server-side session.
 * Called once on app startup — if the session has stored S3 creds
 * (auto-populated after login), they're loaded into memory.
 * Returns true if credentials were restored.
 */
export async function initFromSession(): Promise<boolean> {
  const creds = await fetchSessionCredentials();
  if (!creds) return false;
  activeEndpoint = creds.endpoint || detectDefaultEndpoint();
  activeRegion = creds.region || 'us-east-1';
  activeAccessKeyId = creds.access_key_id;
  activeSecretAccessKey = creds.secret_access_key;
  if (creds.bucket) activeBucket = creds.bucket;
  cachedClient = null;
  return Boolean(creds.access_key_id && creds.secret_access_key);
}

export function getBucket(): string {
  return activeBucket;
}

export function setBucket(name: string) {
  activeBucket = name;
  localStorage.setItem('dg-bucket', name);
}

function getEndpoint(): string {
  return activeEndpoint || detectDefaultEndpoint();
}

export function setEndpoint(url: string) {
  activeEndpoint = url;
  cachedClient = null;
}

export function getCredentials(): { accessKeyId: string; secretAccessKey: string } {
  return {
    accessKeyId: activeAccessKeyId,
    secretAccessKey: activeSecretAccessKey,
  };
}

export function setCredentials(accessKeyId: string, secretAccessKey: string) {
  activeAccessKeyId = accessKeyId;
  activeSecretAccessKey = secretAccessKey;
  cachedClient = null;
  // Persist to server-side session (fire-and-forget)
  storeSessionCredentials({
    endpoint: activeEndpoint,
    region: activeRegion,
    bucket: activeBucket,
    access_key_id: accessKeyId,
    secret_access_key: secretAccessKey,
  });
}

export function hasCredentials(): boolean {
  return Boolean(activeAccessKeyId && activeSecretAccessKey);
}

/** Clear all credentials from memory and server session. */
export function disconnect() {
  activeAccessKeyId = '';
  activeSecretAccessKey = '';
  activeEndpoint = '';
  cachedClient = null;
  // Remove legacy localStorage keys (migration cleanup)
  localStorage.removeItem('dg-access-key-id');
  localStorage.removeItem('dg-secret-access-key');
  localStorage.removeItem('dg-endpoint');
  clearSessionCredentials();
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

async function sendCommand<T>(command: object, context: string): Promise<T> {
  try {
    return await getClient().send(command as never) as T;
  } catch (err) {
    throw normalizeS3Error(err, context);
  }
}

/**
 * A CommonPrefix is a real folder iff it's non-empty, not equal to the prefix
 * we listed under, and doesn't end in "//". The "//" case shows as a bogus "/"
 * folder in the UI and comes from objects with empty path segments.
 */
function isValidCommonPrefix(prefix: string | undefined, currentPrefix: string): prefix is string {
  return !!prefix && prefix !== currentPrefix && !prefix.endsWith('//');
}

const MAX_LIST_PAGES = 10; // ~10,000 objects max

export async function listObjects(prefix = ''): Promise<ListResult> {
  const allObjects: S3Object[] = [];
  const folderSet = new Set<string>();
  let continuationToken: string | undefined;
  let isTruncated = false;

  for (let page = 0; page < MAX_LIST_PAGES; page++) {
    const resp: ListObjectsV2CommandOutput = await sendCommand<ListObjectsV2CommandOutput>(
      new ListObjectsV2Command({
        Bucket: activeBucket,
        Prefix: prefix || undefined,
        Delimiter: '/',
        ContinuationToken: continuationToken,
      }),
      `List objects in bucket ${activeBucket}`
    );

    for (const cp of resp.CommonPrefixes || []) {
      if (isValidCommonPrefix(cp.Prefix, prefix)) {
        folderSet.add(cp.Prefix);
      }
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

export async function listCommonPrefixes(bucket: string, prefix = ''): Promise<string[]> {
  const cleanBucket = bucket.trim();
  if (!cleanBucket) return [];

  const resp = await sendCommand<ListObjectsV2CommandOutput>(
    new ListObjectsV2Command({
      Bucket: cleanBucket,
      Prefix: prefix || undefined,
      Delimiter: '/',
      MaxKeys: 100,
    }),
    `List prefixes in bucket ${cleanBucket}`
  );

  return (resp.CommonPrefixes || [])
    .map((cp) => cp.Prefix)
    .filter((p): p is string => isValidCommonPrefix(p, prefix));
}

interface HeadObjectShape {
  headers: Record<string, string>;
  storageType?: string;
  storedSize?: number;
}

/**
 * Pure transform from a HeadObject response into the flat header map +
 * DeltaGlider-specific storage hints the InspectorPanel consumes. The DG
 * `dg-note` / `dg-delta-size` user-metadata is surfaced as the
 * `x-amz-storage-type` / `x-deltaglider-stored-size` headers.
 */
function formatHeadObjectResponse(resp: HeadObjectCommandOutput): HeadObjectShape {
  const headers: Record<string, string> = {};

  if (resp.ContentType) headers['content-type'] = resp.ContentType;
  if (resp.ContentLength !== undefined) headers['content-length'] = String(resp.ContentLength);
  if (resp.ETag) headers['etag'] = resp.ETag;
  if (resp.LastModified) headers['last-modified'] = resp.LastModified.toUTCString();

  const meta = resp.Metadata || {};
  for (const [k, v] of Object.entries(meta)) {
    headers[`x-amz-meta-${k}`] = v;
  }
  if (resp.StorageClass) headers['x-amz-storage-class'] = resp.StorageClass;

  const storageType = meta['dg-note'] || undefined;
  if (storageType) headers['x-amz-storage-type'] = storageType;
  const storedStr = meta['dg-delta-size'];
  if (storedStr) headers['x-deltaglider-stored-size'] = storedStr;

  return {
    headers,
    storageType,
    storedSize: storedStr ? parseInt(storedStr, 10) : undefined,
  };
}

/** Fetch HEAD metadata for a single object (called lazily by InspectorPanel). */
export async function headObject(key: string): Promise<HeadObjectShape> {
  const headResp = await sendCommand<HeadObjectCommandOutput>(
    new HeadObjectCommand({ Bucket: activeBucket, Key: key }),
    `Read object metadata for ${key}`,
  );
  return formatHeadObjectResponse(headResp);
}

export interface UploadTelemetry {
  key: string;
  status: UploadStatus;
  loadedBytes: number;
  totalBytes: number;
  percent: number;
  speedBytesPerSec: number;
  partSize: number;
  queueSize: number;
  totalParts: number;
  completedParts: number;
  inFlightParts: number;
  activeConnections: number;
  currentPart: number | null;
  elapsedMs: number;
  updatedAtMs: number;
}

interface UploadProgressEvent {
  loaded?: number;
  total?: number;
  part?: number;
}

interface UploadObjectOptions {
  onTelemetry?: (telemetry: UploadTelemetry) => void;
  signal?: AbortSignal;
  partSize?: number;
  queueSize?: number;
}

function emitTelemetry(
  cb: ((telemetry: UploadTelemetry) => void) | undefined,
  telemetry: UploadTelemetry,
): UploadTelemetry {
  if (cb) cb(telemetry);
  return telemetry;
}

function nowMs(): number {
  // Keep telemetry on wall clock so UI elapsed-time math remains stable
  // across refreshes and browser timing-source differences.
  return Date.now();
}

export async function uploadObject(
  key: string,
  data: Blob | ArrayBuffer,
  options: UploadObjectOptions = {},
): Promise<void> {
  const body = data instanceof Blob ? data : new Uint8Array(data);
  const contentType = data instanceof Blob && data.type ? data.type : 'application/octet-stream';
  const totalBytes = data instanceof Blob ? data.size : data.byteLength;
  const partSize = options.partSize ?? DEFAULT_UPLOAD_PART_SIZE;
  const queueSize = options.queueSize ?? DEFAULT_UPLOAD_QUEUE_SIZE;
  const totalParts = estimateTotalParts(totalBytes, partSize);
  const startedAtMs = nowMs();
  let lastCurrentPart: number | null = null;
  let samples: ThroughputSample[] = [{ atMs: startedAtMs, loadedBytes: 0 }];
  let lastTelemetry: UploadTelemetry = {
    key,
    status: 'queued',
    loadedBytes: 0,
    totalBytes,
    percent: 0,
    speedBytesPerSec: 0,
    partSize,
    queueSize,
    totalParts,
    completedParts: 0,
    inFlightParts: 0,
    activeConnections: 0,
    currentPart: null,
    elapsedMs: 0,
    updatedAtMs: startedAtMs,
  };

  try {
    emitTelemetry(options.onTelemetry, lastTelemetry);
    // Managed upload uses multipart automatically for large payloads and
    // retries failed parts individually (critical for long uploads through
    // flaky gateways/proxies).
    const upload = new Upload({
      client: getClient(),
      params: {
        Bucket: activeBucket,
        Key: key,
        Body: body,
        ContentType: contentType,
      },
      partSize,
      queueSize,
      leavePartsOnError: false,
    });
    if (options.signal) {
      options.signal.addEventListener('abort', () => {
        void upload.abort();
      }, { once: true });
    }
    upload.on('httpUploadProgress', (progress: UploadProgressEvent) => {
      const atMs = nowMs();
      const loadedBytes = Math.max(0, progress.loaded ?? 0);
      const resolvedTotalBytes = Math.max(totalBytes, progress.total ?? 0);
      if (typeof progress.part === 'number') {
        lastCurrentPart = progress.part;
      }
      samples = appendThroughputSample(samples, { atMs, loadedBytes });
      const speedBytesPerSec = movingAverageSpeedBps(samples);
      const completedParts = estimateCompletedParts(loadedBytes, resolvedTotalBytes, partSize);
      const status: UploadStatus = loadedBytes >= resolvedTotalBytes && resolvedTotalBytes > 0
        ? 'completing'
        : 'uploading';
      const inFlightParts = estimateInFlightParts(status, totalParts, completedParts, queueSize);
      lastTelemetry = emitTelemetry(options.onTelemetry, {
        key,
        status,
        loadedBytes,
        totalBytes: resolvedTotalBytes,
        percent: clampPercent(
          resolvedTotalBytes > 0 ? (loadedBytes / resolvedTotalBytes) * 100 : 0,
        ),
        speedBytesPerSec,
        partSize,
        queueSize,
        totalParts,
        completedParts,
        inFlightParts,
        activeConnections: inFlightParts,
        currentPart: lastCurrentPart,
        elapsedMs: Math.max(0, atMs - startedAtMs),
        updatedAtMs: atMs,
      });
    });

    await upload.done();
    const atMs = nowMs();
    lastTelemetry = emitTelemetry(options.onTelemetry, {
      ...lastTelemetry,
      status: 'success',
      loadedBytes: totalBytes,
      totalBytes,
      percent: 100,
      completedParts: totalParts,
      inFlightParts: 0,
      activeConnections: 0,
      speedBytesPerSec: Math.max(
        lastTelemetry.speedBytesPerSec,
        movingAverageSpeedBps(appendThroughputSample(samples, { atMs, loadedBytes: totalBytes })),
      ),
      elapsedMs: Math.max(0, atMs - startedAtMs),
      updatedAtMs: atMs,
    });
  } catch (err) {
    const atMs = nowMs();
    if (options.signal?.aborted) {
      emitTelemetry(options.onTelemetry, {
        ...lastTelemetry,
        status: 'cancelled',
        inFlightParts: 0,
        activeConnections: 0,
        elapsedMs: Math.max(0, atMs - startedAtMs),
        updatedAtMs: atMs,
      });
      return;
    }
    emitTelemetry(options.onTelemetry, {
      ...lastTelemetry,
      status: 'error',
      inFlightParts: 0,
      activeConnections: 0,
      elapsedMs: Math.max(0, atMs - startedAtMs),
      updatedAtMs: atMs,
    });
    throw normalizeS3Error(err, `Upload object ${key}`);
  }
}

export async function deleteObject(key: string): Promise<void> {
  await sendCommand(
    new DeleteObjectCommand({ Bucket: activeBucket, Key: key }),
    `Delete object ${key}`,
  );
}

/** Drain a browser ReadableStream into a single Blob. */
async function readStreamToBlob(stream: ReadableStream<Uint8Array>): Promise<Blob> {
  const reader = stream.getReader();
  const chunks: BlobPart[] = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    if (value) chunks.push(value as unknown as BlobPart);
  }
  return new Blob(chunks);
}

export async function downloadObject(key: string): Promise<Blob> {
  const resp = await sendCommand<GetObjectCommandOutput>(
    new GetObjectCommand({ Bucket: activeBucket, Key: key }),
    `Download object ${key}`,
  );
  if (!resp.Body) throw new Error('Empty response body');
  // resp.Body is a ReadableStream in the browser
  return readStreamToBlob(resp.Body as ReadableStream<Uint8Array>);
}

export async function getPresignedUrl(key: string, expiresInSeconds = 7 * 24 * 3600 - 1): Promise<string> {
  try {
    const client = getClient();
    const command = new GetObjectCommand({ Bucket: activeBucket, Key: key });
    return await getSignedUrl(client, command, { expiresIn: expiresInSeconds });
  } catch (err) {
    throw normalizeS3Error(err, `Create presigned URL for ${key}`);
  }
}

export function getObjectUrl(key: string): string {
  return `${getEndpoint()}/${activeBucket}/${encodeURIComponent(key)}`;
}

// ── Connection testing ──

interface TestConnectionResult {
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
    return { ok: false, error: normalizeS3Error(e, 'Test S3 connection').message };
  }
}

// ── Bucket operations ──

type ListBucketsOptions = {
  /**
   * When false, skips loading extra bucket details from Settings (403 without administrator sign-in).
   * @default true
   */
  includeOrigins?: boolean;
};

// Five independent callers (Sidebar, DestinationPickerModal, AnalyticsSection,
// BucketScanCard, PermissionEditor) each fire listBuckets on mount/open. Share
// one in-flight promise + a short-lived result so near-simultaneous callers
// cost ONE ListBuckets + origins pair instead of five.
const LIST_BUCKETS_TTL_MS = 5_000;
let listBucketsShared: { at: number; key: string; promise: Promise<BucketInfo[]> } | null = null;

/** Drop the shared listBuckets result (call after create/delete bucket). */
export function invalidateListBucketsCache(): void {
  listBucketsShared = null;
}

export async function listBuckets(opts?: ListBucketsOptions): Promise<BucketInfo[]> {
  const key = opts?.includeOrigins === false ? 'no-origins' : 'origins';
  const now = Date.now();
  if (
    listBucketsShared &&
    listBucketsShared.key === key &&
    now - listBucketsShared.at < LIST_BUCKETS_TTL_MS
  ) {
    return listBucketsShared.promise;
  }
  const promise = listBucketsUncached(opts);
  listBucketsShared = { at: now, key, promise };
  // A failed listing must not poison the window — let the next caller retry.
  promise.catch(() => {
    if (listBucketsShared?.promise === promise) listBucketsShared = null;
  });
  return promise;
}

async function listBucketsUncached(opts?: ListBucketsOptions): Promise<BucketInfo[]> {
  const includeOrigins = opts?.includeOrigins !== false;
  const originsPromise = includeOrigins
    ? getBucketOrigins().catch(() => null)
    : Promise.resolve(null);
  // allSettled, NOT all: on a TOTAL backend failure the S3 list rejects, but
  // the admin origins endpoint still returns the config-declared roster as
  // unavailable placeholders — we must render those, not throw them away.
  const [respSettled, origins] = await Promise.all([
    sendCommand<ListBucketsCommandOutput>(new ListBucketsCommand({}), 'List buckets').then(
      (v) => ({ ok: true as const, v }),
      (e) => ({ ok: false as const, e }),
    ),
    originsPromise,
  ]);
  const toBackend = (o: NonNullable<typeof origins>['buckets'][number]) => ({
    backendName: o.backend_name || undefined,
    backendType: o.backend_type || undefined,
    backendEndpoint: o.backend_endpoint || undefined,
    backendRegion: o.backend_region || undefined,
    backendPath: o.backend_path || undefined,
    realBucket: o.real_bucket || undefined,
  });
  if (!respSettled.ok) {
    const flagged = (origins?.buckets || []).filter((b) => b.unavailable);
    if (flagged.length === 0) throw respSettled.e;
    return flagged
      .map((o) => ({
        name: o.name,
        creationDate: o.creation_date || '',
        backend: toBackend(o),
        unavailable: o.unavailable || undefined,
      }))
      .sort((a, b) => numericCompare(a.name, b.name));
  }
  const resp = respSettled.v;
  const originByName = new Map((origins?.buckets || []).map((b) => [b.name, b]));
  const s3Names = new Set((resp.Buckets || []).map((b) => b.Name || ''));
  const merged: BucketInfo[] = (resp.Buckets || []).map((b) => {
    const name = b.Name || '';
    const origin = originByName.get(name);
    return {
      name,
      creationDate: b.CreationDate?.toISOString() || origin?.creation_date || '',
      backend: origin ? toBackend(origin) : undefined,
      unavailable: origin?.unavailable || undefined,
    };
  });
  // Union in buckets that the admin origins endpoint reports but S3 ListBuckets
  // did NOT return — these are config-declared buckets whose backend is
  // unreachable (503/throttle). We must SHOW them (flagged unavailable), never
  // drop them, so the operator sees the full roster + why one is dark.
  for (const o of origins?.buckets || []) {
    if (s3Names.has(o.name)) continue;
    if (!o.unavailable) continue; // only surface origin-only rows when they're flagged unavailable
    merged.push({
      name: o.name,
      creationDate: o.creation_date || '',
      backend: toBackend(o),
      unavailable: o.unavailable,
    });
  }
  merged.sort((a, b) => numericCompare(a.name, b.name));
  return merged;
}

export async function createBucket(name: string, backendName?: string): Promise<void> {
  if (backendName) {
    await createBucketOnBackend(name, backendName);
    invalidateListBucketsCache();
    return;
  }
  await sendCommand(new CreateBucketCommand({ Bucket: name }), `Create bucket ${name}`);
  invalidateListBucketsCache();
}

export async function deleteBucket(name: string): Promise<void> {
  await sendCommand(
    new DeleteBucketCommand({ Bucket: name }),
    `Delete bucket ${name}`,
  );
  invalidateListBucketsCache();
}

type MultipartUploadRef = {
  key: string;
  uploadId: string;
};

async function listMultipartUploadsPage(
  bucket: string,
  keyMarker?: string,
  uploadIdMarker?: string,
): Promise<{ uploads: MultipartUploadRef[]; isTruncated: boolean; nextKeyMarker?: string; nextUploadIdMarker?: string }> {
  const resp = await sendCommand<ListMultipartUploadsCommandOutput>(
    new ListMultipartUploadsCommand({
      Bucket: bucket,
      KeyMarker: keyMarker,
      UploadIdMarker: uploadIdMarker,
      MaxUploads: 1000,
    }),
    `List multipart uploads in bucket ${bucket}`,
  );
  const uploads = (resp.Uploads || [])
    .map((upload) => ({
      key: upload.Key || '',
      uploadId: upload.UploadId || '',
    }))
    .filter((upload) => Boolean(upload.key) && Boolean(upload.uploadId));
  return {
    uploads,
    isTruncated: Boolean(resp.IsTruncated),
    nextKeyMarker: resp.NextKeyMarker,
    nextUploadIdMarker: resp.NextUploadIdMarker,
  };
}

export async function countMultipartUploads(bucket: string): Promise<number> {
  let count = 0;
  let keyMarker: string | undefined;
  let uploadIdMarker: string | undefined;
  for (;;) {
    const page = await listMultipartUploadsPage(bucket, keyMarker, uploadIdMarker);
    count += page.uploads.length;
    if (!page.isTruncated) break;
    keyMarker = page.nextKeyMarker;
    uploadIdMarker = page.nextUploadIdMarker;
    if (!keyMarker && !uploadIdMarker) break;
  }
  return count;
}

export async function abortAllMultipartUploads(bucket: string): Promise<{ aborted: number; remaining: number }> {
  let totalAborted = 0;
  // Retry in rounds so pagination remains stable while we mutate upload state.
  // If a round makes no progress, remaining uploads are likely in transient
  // states (or concurrently recreated), so we stop deterministically.
  for (let round = 0; round < 8; round++) {
    const pending: MultipartUploadRef[] = [];
    let keyMarker: string | undefined;
    let uploadIdMarker: string | undefined;
    for (;;) {
      const page = await listMultipartUploadsPage(bucket, keyMarker, uploadIdMarker);
      pending.push(...page.uploads);
      if (!page.isTruncated) break;
      keyMarker = page.nextKeyMarker;
      uploadIdMarker = page.nextUploadIdMarker;
      if (!keyMarker && !uploadIdMarker) break;
    }
    if (pending.length === 0) {
      return { aborted: totalAborted, remaining: 0 };
    }
    let abortedThisRound = 0;
    for (const upload of pending) {
      try {
        await sendCommand(
          new AbortMultipartUploadCommand({
            Bucket: bucket,
            Key: upload.key,
            UploadId: upload.uploadId,
          }),
          `Abort multipart upload ${upload.uploadId}`,
        );
        abortedThisRound += 1;
      } catch {
        // Best effort: the upload may have already completed/aborted concurrently.
      }
    }
    totalAborted += abortedThisRound;
    if (abortedThisRound === 0) {
      return { aborted: totalAborted, remaining: pending.length };
    }
  }
  return { aborted: totalAborted, remaining: await countMultipartUploads(bucket) };
}

