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
import { codePointCompare, detectDefaultEndpoint, numericCompare } from './utils';
import { createBucketOnBackend, getBucketOrigins } from './adminApi';
import { normalizeS3Error } from './errorHandling';
import { queryClient } from './queries/client';
import { qk } from './queries/keys';
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
} from './sessionApi';
import { readStorage, removeStorage, writeStorage } from './safeStorage';

// ── In-memory credential store (never persisted to localStorage) ──

let activeBucket = readStorage('dg-bucket') || '';
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
  writeStorage('dg-bucket', name);
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

/**
 * Switch the in-memory credentials (synchronously, before the first await) and
 * persist them to the server-side session, which is what survives a reload.
 * Resolves false (and logs) when that save failed: the connection works now
 * but a reload will ask for a sign-in again. Callers that can, tell the user.
 */
export async function setCredentials(accessKeyId: string, secretAccessKey: string): Promise<boolean> {
  activeAccessKeyId = accessKeyId;
  activeSecretAccessKey = secretAccessKey;
  cachedClient = null;
  const saved = await storeSessionCredentials({
    endpoint: activeEndpoint,
    region: activeRegion,
    bucket: activeBucket,
    access_key_id: accessKeyId,
    secret_access_key: secretAccessKey,
  });
  if (!saved) {
    console.warn('Could not save S3 credentials to the server session; a reload will need a new sign-in.');
  }
  return saved;
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
  removeStorage('dg-access-key-id');
  removeStorage('dg-secret-access-key');
  removeStorage('dg-endpoint');
  // Local only: the caller clears the server-side copy BEFORE it logs out
  // (after the logout that request is a 401). See handleLogout in App.tsx.
}

let cachedClient: S3Client | null = null;

/** The response headers in which the proxy tells a writer how it stored the object. */
const WRITE_FACT_HEADERS = [
  'x-amz-storage-type',
  'x-deltaglider-stored-size',
  'x-deltaglider-ref-sha256',
  'x-deltaglider-baseline',
] as const;
/** The write-fact headers of a write, by lower-case name (absent = not sent). */
export type WriteFacts = Partial<Record<(typeof WRITE_FACT_HEADERS)[number], string>>;
const WRITE_FACTS = Symbol('dgWriteFacts');
const WRITE_COMMANDS = new Set(['PutObjectCommand', 'CompleteMultipartUploadCommand']);

/**
 * Keep the write-fact response headers of PutObject and
 * CompleteMultipartUpload on the command output (under a symbol, which the
 * lib-storage `Upload` result keeps: it spreads the PutObject output). Runs
 * outside the deserializer, which leaves the raw response next to the output.
 */
function captureWriteFacts(client: S3Client) {
  client.middlewareStack.add(
    ((next: (args: unknown) => Promise<{ output: object; response: unknown }>, context: { commandName?: string }) =>
      async (args: unknown) => {
        const result = await next(args);
        if (WRITE_COMMANDS.has(context.commandName ?? '')) {
          const headers = (result.response as { headers?: Record<string, string> }).headers ?? {};
          const facts: WriteFacts = {};
          for (const name of WRITE_FACT_HEADERS) {
            const value = headers[name];
            if (value !== undefined) facts[name] = value;
          }
          (result.output as Record<symbol, WriteFacts>)[WRITE_FACTS] = facts;
        }
        return result;
      }) as never,
    { step: 'deserialize', priority: 'high', name: 'dgWriteFacts' },
  );
}

function getClient(): S3Client {
  if (!cachedClient) {
    const creds = getCredentials();
    cachedClient = new S3Client({
      endpoint: getEndpoint(),
      region: activeRegion,
      credentials: { accessKeyId: creds.accessKeyId, secretAccessKey: creds.secretAccessKey },
      forcePathStyle: true,
    });
    captureWriteFacts(cachedClient);
  }
  return cachedClient;
}

async function sendCommand<T>(command: object, context: string, abortSignal?: AbortSignal): Promise<T> {
  try {
    return await getClient().send(command as never, abortSignal ? { abortSignal } : undefined) as T;
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

/** The storage facts of one listed object, as HeadObject would show them. */
interface ListFacts {
  storageType?: string;
  storedSize?: number;
}

/**
 * The storage facts of each `<Contents>` of a `metadata=true` ListObjectsV2
 * body, by key. The proxy lists them in `<UserMetadata>` with the names
 * HeadObject uses: `x-amz-meta-dg-note` (the storage type) and, for a delta,
 * `x-amz-meta-dg-delta-size` (the stored size). An entry without them (a
 * facts miss, an anonymous reader) is absent from the map.
 */
export function parseListFacts(xml: string): Map<string, ListFacts> {
  const facts = new Map<string, ListFacts>();
  const doc = new DOMParser().parseFromString(xml, 'application/xml');
  for (const contents of Array.from(doc.getElementsByTagName('Contents'))) {
    const key = Array.from(contents.children).find((c) => c.tagName === 'Key')?.textContent;
    const userMetadata = Array.from(contents.children).find((c) => c.tagName === 'UserMetadata');
    if (key == null || !userMetadata) continue;
    const meta: Record<string, string> = {};
    for (const item of Array.from(userMetadata.getElementsByTagName('Items'))) {
      const name = item.getElementsByTagName('Key')[0]?.textContent;
      const value = item.getElementsByTagName('Value')[0]?.textContent;
      if (name && value != null) meta[name.toLowerCase()] = value;
    }
    const storageType = meta['x-amz-meta-dg-note'] || undefined;
    if (!storageType) continue;
    const size = meta['x-amz-meta-dg-delta-size'];
    facts.set(key, { storageType, storedSize: size ? parseInt(size, 10) : undefined });
  }
  return facts;
}

/**
 * A ListObjectsV2 command that asks for the storage facts of every entry
 * (`metadata=true`) and hands them to `onFacts`. The query parameter is added
 * at the build step, before the signer signs the canonical query. The body
 * is read before the SDK's deserializer, which drops unknown elements such
 * as `<UserMetadata>`, and handed on as a fresh stream.
 */
// The SDK's middleware types live in @smithy/types (not a direct dependency):
// the two handlers below are typed by the shape they use, and cast at `add`.
interface HandlerArgs { input: unknown; request?: unknown }
type Next = (args: HandlerArgs) => Promise<{ output: unknown; response: unknown }>;

function listWithFacts(
  input: ConstructorParameters<typeof ListObjectsV2Command>[0],
  onFacts: (facts: Map<string, ListFacts>) => void,
): ListObjectsV2Command {
  const command = new ListObjectsV2Command(input);
  const addQuery = (next: Next) => async (args: HandlerArgs) => {
    const request = args.request as { query?: Record<string, string> };
    request.query = { ...request.query, metadata: 'true' };
    return next(args);
  };
  const readFacts = (next: Next) => async (args: HandlerArgs) => {
    const result = await next(args);
    const response = result.response as { statusCode?: number; body?: unknown };
    if (response.statusCode === 200 && response.body != null) {
      const text = await new Response(response.body as BodyInit).text();
      onFacts(parseListFacts(text));
      response.body = new Response(text).body;
    }
    return result;
  };
  command.middlewareStack.add(addQuery as never, { step: 'build', name: 'dgListMetadataQuery' });
  command.middlewareStack.addRelativeTo(readFacts as never, {
    relation: 'after',
    toMiddleware: 'deserializerMiddleware',
    name: 'dgListFacts',
  });
  return command;
}

/**
 * List the folder `prefix`: up to `maxPages` pages of 1,000 entries (files
 * and sub-folders). `isTruncated` = the 10-page cap of a full listing cut
 * it; `continuesAfter` = the listing stopped with more entries after this
 * key (the cap, or `maxPages`).
 */
export async function listObjects(prefix = '', maxPages = MAX_LIST_PAGES): Promise<ListResult> {
  const allObjects: S3Object[] = [];
  const folderSet = new Set<string>();
  let continuationToken: string | undefined;
  let isTruncated = false;
  let continuesAfter: string | undefined;
  const pages = Math.min(maxPages, MAX_LIST_PAGES);
  let listed = 0;

  for (let page = 0; page < pages; page++) {
    listed++;
    let facts = new Map<string, ListFacts>();
    const resp: ListObjectsV2CommandOutput = await sendCommand<ListObjectsV2CommandOutput>(
      listWithFacts(
        {
          Bucket: activeBucket,
          Prefix: prefix || undefined,
          Delimiter: '/',
          ContinuationToken: continuationToken,
        },
        (parsed) => { facts = parsed; },
      ),
      `List objects in bucket ${activeBucket}`
    );

    let lastListed = '';
    for (const cp of resp.CommonPrefixes || []) {
      if (cp.Prefix && codePointCompare(cp.Prefix, lastListed) > 0) lastListed = cp.Prefix;
      if (isValidCommonPrefix(cp.Prefix, prefix)) {
        folderSet.add(cp.Prefix);
      }
    }

    for (const o of resp.Contents || []) {
      const key = o.Key || '';
      if (codePointCompare(key, lastListed) > 0) lastListed = key;
      const listed = facts.get(key);
      allObjects.push({
        key,
        size: o.Size || 0,
        lastModified: o.LastModified?.toISOString() || '',
        etag: o.ETag || '',
        storageType: listed?.storageType,
        storedSize: listed?.storedSize,
        headers: {},
      });
    }

    if (!resp.IsTruncated) break;
    continuationToken = resp.NextContinuationToken;
    if (!continuationToken) break;
    if (page === pages - 1) {
      continuesAfter = lastListed;
      // The 10-page cap of a full listing: say so.
      if (pages === MAX_LIST_PAGES) isTruncated = true;
    }
  }

  return { objects: allObjects, folders: Array.from(folderSet), isTruncated, continuesAfter, pages: listed };
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
/** `bucket` defaults to the active one; pass it when the caller captured it earlier. */
/** `signal` aborts the request (the browser's table drops a folder's HEADs when the user leaves it). */
export async function headObject(key: string, bucket = activeBucket, signal?: AbortSignal): Promise<HeadObjectShape> {
  const headResp = await sendCommand<HeadObjectCommandOutput>(
    new HeadObjectCommand({ Bucket: bucket, Key: key }),
    `Read object metadata for ${key}`,
    signal,
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
  /** Target bucket; defaults to the active one. The upload queue captures it at enqueue time. */
  bucket?: string;
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

/**
 * Upload one object (multipart when large). Resolves with the storage facts
 * the proxy sent on the PutObject or CompleteMultipartUpload response, or
 * undefined when the upload was cancelled.
 */
export async function uploadObject(
  key: string,
  data: Blob | ArrayBuffer,
  options: UploadObjectOptions = {},
): Promise<WriteFacts | undefined> {
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

  // An abort before this point has no listener to fire: honour it now.
  options.signal?.throwIfAborted();
  try {
    emitTelemetry(options.onTelemetry, lastTelemetry);
    // Managed upload uses multipart automatically for large payloads and
    // retries failed parts individually (critical for long uploads through
    // flaky gateways/proxies).
    const upload = new Upload({
      client: getClient(),
      params: {
        Bucket: options.bucket ?? activeBucket,
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

    const output = await upload.done();
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
    return (output as unknown as Record<symbol, WriteFacts | undefined>)[WRITE_FACTS] ?? {};
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

/**
 * A short-lived presigned GET that makes the browser SAVE the object under
 * `filename` (response-content-disposition). The browser's own downloader
 * streams it to disk: no Blob in page memory, whatever the object size.
 */
export async function getDownloadUrl(key: string, filename: string, expiresInSeconds = 300): Promise<string> {
  const ascii = filename.replace(/[^\x20-\x7e]|["\\]/g, '_');
  const disposition = `attachment; filename="${ascii}"; filename*=UTF-8''${encodeURIComponent(filename)}`;
  try {
    const command = new GetObjectCommand({ Bucket: activeBucket, Key: key, ResponseContentDisposition: disposition });
    return await getSignedUrl(getClient(), command, { expiresIn: expiresInSeconds });
  } catch (err) {
    throw normalizeS3Error(err, `Create download URL for ${key}`);
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

/** Drop the shared listBuckets result and the bucket origins (call after
 *  create/delete bucket). */
export function invalidateListBucketsCache(): void {
  listBucketsShared = null;
  void queryClient.invalidateQueries({ queryKey: qk.backends.origins() });
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
  // The bucket origins through the shared react-query entry: the Sidebar,
  // the usage chip and the backends panel read the same one.
  const originsPromise = includeOrigins
    ? queryClient
        // No retry: a session without admin rights gets a 403 at once.
        .fetchQuery({ queryKey: qk.backends.origins(), queryFn: getBucketOrigins, staleTime: 5_000, retry: false })
        .catch(() => null)
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

export async function createBucket(
  name: string,
  backendName?: string,
  /** The bucket's own encryption block (and its confirmation), created in
   *  the same server write as the bucket. Needs `backendName`. */
  opts?: Parameters<typeof createBucketOnBackend>[2],
): Promise<void> {
  if (backendName) {
    await createBucketOnBackend(name, backendName, opts);
    invalidateListBucketsCache();
    return;
  }
  await sendCommand(new CreateBucketCommand({ Bucket: name }), `Create bucket ${name}`);
  invalidateListBucketsCache();
}

/**
 * How many objects the bucket holds, counted up to `cap` in one
 * ListObjectsV2 page. `truncated` means "at least `cap`". Keep `cap` small:
 * the caller asks "is it empty" or "how many, roughly".
 */
export async function countBucketObjects(
  name: string,
  cap: number,
): Promise<{ count: number; truncated: boolean }> {
  const resp = await sendCommand<ListObjectsV2CommandOutput>(
    new ListObjectsV2Command({ Bucket: name, MaxKeys: cap }),
    `Count objects in bucket ${name}`,
  );
  return { count: resp.KeyCount ?? resp.Contents?.length ?? 0, truncated: Boolean(resp.IsTruncated) };
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

