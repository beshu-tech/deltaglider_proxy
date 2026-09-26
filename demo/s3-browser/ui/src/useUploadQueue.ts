import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { getBucket, headObject, listDirectObjects, uploadObject, type UploadTelemetry } from './s3client';
import { uploadSessionStats } from './uploadStats';
import { uploadCreatedBaseline } from './savings';
import {
  clampPercent,
  folderEmptyFromListing,
  startAfterProbe,
  mergeTotalBytes,
  uploadRetryAdvice,
  type UploadRetryAdvice,
  type UploadStatus,
} from './uploadTelemetry';
import { ApiError, normalizeUiError } from './errorHandling';
import { createUploadQueueStore } from './uploadQueueStore';

export interface UploadQueueItem {
  id: string;
  file: File;
  /** Bucket captured when the file was queued: switching buckets later must not redirect it. */
  bucket: string;
  destination: string;
  key: string;
  status: UploadStatus;
  originalSize: number;
  transferredBytes: number;
  totalBytes: number;
  percent: number;
  speedBytesPerSec: number;
  totalParts: number;
  completedParts: number;
  inFlightParts: number;
  activeConnections: number;
  currentPart: number | null;
  startedAtMs: number | null;
  completingSinceMs: number | null;
  updatedAtMs: number | null;
  durationMs: number | null;
  /** Bytes the proxy stored (HEAD after success); undefined = pending/unknown. */
  storedSize?: number;
  /** Set when this upload became its folder's baseline (see uploadStats.ts). */
  baselineKey?: string;
  error?: string;
  /** What to offer after a failure (see `uploadRetryAdvice`). */
  retry?: UploadRetryAdvice;
}

/** Merge one progress event into its queue item (pure). */
function withTelemetry(item: UploadQueueItem, telemetry: UploadTelemetry): UploadQueueItem {
  const nextStatus = telemetry.status;
  const enteringCompleting = nextStatus === 'completing' && item.status !== 'completing';
  return {
    ...item,
    status: nextStatus,
    transferredBytes: telemetry.loadedBytes,
    totalBytes: mergeTotalBytes(telemetry.totalBytes, item.totalBytes),
    percent: clampPercent(telemetry.percent),
    speedBytesPerSec: telemetry.speedBytesPerSec,
    totalParts: telemetry.totalParts,
    completedParts: telemetry.completedParts,
    inFlightParts: telemetry.inFlightParts,
    activeConnections: telemetry.activeConnections,
    currentPart: telemetry.currentPart,
    updatedAtMs: telemetry.updatedAtMs,
    startedAtMs: item.startedAtMs ?? telemetry.updatedAtMs,
    completingSinceMs: enteringCompleting
      ? telemetry.updatedAtMs
      : nextStatus !== 'completing'
        ? null
        : item.completingSinceMs,
    durationMs: telemetry.elapsedMs,
    error: telemetry.status === 'error' ? item.error : undefined,
  };
}

/**
 * At most one React state update per this many ms. Progress events can
 * come hundreds of times per second; the page does not need them all.
 */
const FLUSH_MS = 50;

export default function useUploadQueue(destination: string) {
  // The keyed store is the source of truth; `queue` is its latest snapshot.
  // Updates go to the store at once (O(1)) and reach React batched, at most
  // once per FLUSH_MS (see uploadQueueStore.ts for the cost model).
  const [store] = useState(() => createUploadQueueStore<UploadQueueItem>());
  const [queue, setQueue] = useState<UploadQueueItem[]>([]);
  const flushTimerRef = useRef<number | null>(null);
  const activeUploadsRef = useRef(new Set<string>());
  const controllersRef = useRef(new Map<string, AbortController>());
  // Upload one file at a time. With queueSize=4 parts per file, a single
  // active file already drives ~64 MB of in-flight body buffering;
  // running two files concurrently (8 × 16 MB = 128 MB of body data
  // hitting the proxy + reverse-proxy ingress simultaneously) hit a
  // 60 s 400 from somewhere in the body-buffering chain on prod (see
  // prod logs at 2026-05-08 20:34 UTC, Warp.dmg + bonsai .tar.xz both
  // failed every part at exactly 60 000 ms while a subsequent solo
  // `dg-160mb.bin` upload — same proxy, same prefix — succeeded with
  // all 10 parts at ~43 s each). Keep cross-file concurrency at 1
  // until the root-cause investigation in the spawned task pins down
  // the timer; per-file 4-part queue still governs throughput.
  const maxConcurrentFiles = 1;
  // `${bucket}|${folder}` -> "held no objects before this session's first
  // upload into it". Probed once, before that first upload starts.
  const folderWasEmptyRef = useRef(new Map<string, Promise<boolean | undefined>>());

  useEffect(
    () => () => {
      if (flushTimerRef.current !== null) window.clearTimeout(flushTimerRef.current);
    },
    [],
  );

  const scheduleFlush = useCallback(() => {
    if (flushTimerRef.current !== null) return;
    flushTimerRef.current = window.setTimeout(() => {
      flushTimerRef.current = null;
      setQueue(store.snapshot());
    }, FLUSH_MS);
  }, [store]);

  const patch = useCallback(
    (id: string, fn: (item: UploadQueueItem) => UploadQueueItem) => {
      store.update(id, fn);
      scheduleFlush();
    },
    [store, scheduleFlush],
  );

  const toKey = useCallback((dest: string, file: File): string => {
    const relativePath = (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name;
    const cleanRelativePath = relativePath.replace(/^\/+/, '').replace(/\/{2,}/g, '/');
    return dest ? `${dest}/${cleanRelativePath}` : cleanRelativePath;
  }, []);

  // The next file starts when the previous one settles, from the store,
  // not from a render: the upload rate does not depend on the page.
  // `startRef` breaks the pump <-> start cycle between the two callbacks.
  const startRef = useRef<(item: UploadQueueItem) => void>(() => {});
  const pump = useCallback(() => {
    while (activeUploadsRef.current.size < maxConcurrentFiles) {
      const next = store.nextQueued((item) => item.status === 'queued');
      if (!next) return;
      startRef.current(next);
    }
  }, [store]);

  const startUpload = useCallback((item: UploadQueueItem) => {
    if (activeUploadsRef.current.has(item.id)) return;
    activeUploadsRef.current.add(item.id);
    const controller = new AbortController();
    controllersRef.current.set(item.id, controller);

    patch(item.id, (entry) => ({
      ...entry,
      status: 'uploading',
      error: undefined,
      startedAtMs: Date.now(),
      completingSinceMs: null,
      updatedAtMs: Date.now(),
    }));

    const folder = item.key.includes('/') ? item.key.slice(0, item.key.lastIndexOf('/')) : '';
    const folderKey = `${item.bucket}|${folder}`;
    if (!folderWasEmptyRef.current.has(folderKey)) {
      folderWasEmptyRef.current.set(
        folderKey,
        listDirectObjects(item.bucket, folder ? `${folder}/` : '', 100)
          .then(folderEmptyFromListing)
          .catch(() => undefined),
      );
    }
    const folderWasEmpty = folderWasEmptyRef.current.get(folderKey)!;

    // Cancel during the probe must stop the upload before it starts.
    startAfterProbe(folderWasEmpty, controller.signal, () => uploadObject(item.key, item.file, {
      bucket: item.bucket,
      signal: controller.signal,
      onTelemetry: (telemetry) => patch(item.id, (entry) => withTelemetry(entry, telemetry)),
    }))
      .then(() => {
        patch(item.id, (entry) => ({
          ...entry,
          status: entry.status === 'cancelled' ? 'cancelled' : 'success',
          percent: 100,
          transferredBytes: entry.totalBytes,
          completedParts: entry.totalParts,
          inFlightParts: 0,
          activeConnections: 0,
          completingSinceMs: null,
        }));
        // The browser only knows logical bytes; the proxy decides delta vs
        // passthrough. HEAD the object for the stored size the inspector shows
        // (`dg-delta-size` for a delta, else the full object). A failed HEAD
        // leaves it unknown, and the page shows "—" instead of a guess.
        Promise.all([headObject(item.key, item.bucket), folderWasEmpty])
          .then(([{ headers, storedSize }, wasEmpty]) => {
            const baselineKey = uploadCreatedBaseline(headers, wasEmpty)
              ? `${folderKey}|${headers['x-amz-meta-dg-ref-sha256']}`
              : undefined;
            patch(item.id, (entry) =>
              entry.status === 'success'
                ? { ...entry, storedSize: storedSize ?? entry.originalSize, baselineKey }
                : entry,
            );
          })
          .catch(() => {});
      })
      .catch((err) => {
        if (controller.signal.aborted) {
          patch(item.id, (entry) => ({
            ...entry,
            status: 'cancelled',
            inFlightParts: 0,
            activeConnections: 0,
            speedBytesPerSec: 0,
            completingSinceMs: null,
          }));
          return;
        }
        patch(item.id, (entry) => ({
          ...entry,
          status: 'error',
          error: normalizeUiError(err, 'Upload failed'),
          retry: err instanceof ApiError
            ? uploadRetryAdvice(err.status, err.code, err.detail)
            : 'retry',
          inFlightParts: 0,
          activeConnections: 0,
          speedBytesPerSec: 0,
          completingSinceMs: null,
        }));
      })
      .finally(() => {
        activeUploadsRef.current.delete(item.id);
        controllersRef.current.delete(item.id);
        pump();
      });
  }, [patch, pump]);

  useEffect(() => {
    startRef.current = startUpload;
  }, [startUpload]);

  const addFiles = useCallback((files: FileList | File[]) => {
    const cleanDest = destination.replace(/^\/+/, '').replace(/\/+$/, '').replace(/\/{2,}/g, '/');
    const items: UploadQueueItem[] = Array.from(files).map((file) => ({
      id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      file,
      bucket: getBucket(),
      destination: cleanDest,
      key: toKey(cleanDest, file),
      status: 'queued',
      originalSize: file.size,
      transferredBytes: 0,
      totalBytes: file.size,
      percent: 0,
      speedBytesPerSec: 0,
      totalParts: 0,
      completedParts: 0,
      inFlightParts: 0,
      activeConnections: 0,
      currentPart: null,
      startedAtMs: null,
      completingSinceMs: null,
      updatedAtMs: null,
      durationMs: null,
    }));
    store.add(items);
    scheduleFlush();
    pump();
  }, [destination, toKey, store, scheduleFlush, pump]);

  const clearCompleted = useCallback(() => {
    store.remove((item) => item.status === 'success' || item.status === 'error' || item.status === 'cancelled');
    scheduleFlush();
  }, [store, scheduleFlush]);

  const cancelUpload = useCallback((id: string) => {
    const controller = controllersRef.current.get(id);
    if (controller) {
      controller.abort();
      return;
    }
    patch(id, (item) =>
      item.status === 'queued' ? { ...item, status: 'cancelled' as const, speedBytesPerSec: 0 } : item,
    );
  }, [patch]);

  const retryUpload = useCallback((id: string) => {
    patch(id, (item) => ({
      ...item,
      status: 'queued',
      transferredBytes: 0,
      percent: 0,
      speedBytesPerSec: 0,
      completedParts: 0,
      inFlightParts: 0,
      activeConnections: 0,
      currentPart: null,
      error: undefined,
      retry: undefined,
      startedAtMs: null,
      completingSinceMs: null,
      updatedAtMs: null,
      durationMs: null,
      storedSize: undefined,
      baselineKey: undefined,
    }));
    store.requeue(id);
    pump();
  }, [patch, store, pump]);

  const pendingCount = queue.filter(
    (i) => i.status === 'queued' || i.status === 'uploading' || i.status === 'completing',
  ).length;
  const activeCount = queue.filter((i) => i.status === 'uploading' || i.status === 'completing').length;

  // Stored size and savings come from the per-upload HEADs; both stay null
  // (shown as "—") until every completed upload has one. Savings % goes
  // through `src/savings.ts` (object view, decimal cap at 99.9).
  const stats = useMemo(() => uploadSessionStats(queue), [queue]);

  return {
    queue,
    stats,
    pendingCount,
    activeCount,
    addFiles,
    clearCompleted,
    cancelUpload,
    retryUpload,
  };
}
