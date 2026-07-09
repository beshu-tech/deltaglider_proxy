import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { uploadObject, type UploadTelemetry } from './s3client';
import { summarizeObjectSavings } from './savings';
import { clampPercent, mergeTotalBytes, type UploadStatus } from './uploadTelemetry';
import { normalizeUiError } from './errorHandling';

export interface UploadQueueItem {
  id: string;
  file: File;
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
  error?: string;
}

interface UploadStats {
  uploaded: number;
  originalSize: number;
  storedSize: number;
}

export default function useUploadQueue(destination: string) {
  const [queue, setQueue] = useState<UploadQueueItem[]>([]);
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
  const [tick, setTick] = useState(0);

  const toKey = useCallback((dest: string, file: File): string => {
    const relativePath = (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name;
    const cleanRelativePath = relativePath.replace(/^\/+/, '').replace(/\/{2,}/g, '/');
    return dest ? `${dest}/${cleanRelativePath}` : cleanRelativePath;
  }, []);

  const addFiles = useCallback((files: FileList | File[]) => {
    const cleanDest = destination.replace(/^\/+/, '').replace(/\/+$/, '').replace(/\/{2,}/g, '/');
    const items: UploadQueueItem[] = Array.from(files).map((file) => ({
      id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      file,
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
    setQueue((prev) => [...prev, ...items]);
  }, [destination, toKey]);

  const applyTelemetry = useCallback((id: string, telemetry: UploadTelemetry) => {
    setQueue((prev) =>
      prev.map((item) =>
        item.id !== id
          ? item
          : (() => {
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
            })(),
      ),
    );
  }, []);

  const startUpload = useCallback((item: UploadQueueItem) => {
    if (activeUploadsRef.current.has(item.id)) return;
    activeUploadsRef.current.add(item.id);
    const controller = new AbortController();
    controllersRef.current.set(item.id, controller);

    setQueue((prev) =>
      prev.map((entry) =>
        entry.id !== item.id
          ? entry
          : {
              ...entry,
              status: 'uploading',
              error: undefined,
              startedAtMs: Date.now(),
              completingSinceMs: null,
              updatedAtMs: Date.now(),
            },
      ),
    );

    uploadObject(item.key, item.file, {
      signal: controller.signal,
      onTelemetry: (telemetry) => applyTelemetry(item.id, telemetry),
    })
      .then(() => {
        setQueue((prev) =>
          prev.map((entry) =>
            entry.id !== item.id
              ? entry
              : {
                  ...entry,
                  status: entry.status === 'cancelled' ? 'cancelled' : 'success',
                  percent: 100,
                  transferredBytes: entry.totalBytes,
                  completedParts: entry.totalParts,
                  inFlightParts: 0,
                  activeConnections: 0,
                  completingSinceMs: null,
                },
          ),
        );
      })
      .catch((err) => {
        if (controller.signal.aborted) {
          setQueue((prev) =>
            prev.map((entry) =>
              entry.id === item.id
                ? {
                    ...entry,
                    status: 'cancelled',
                    inFlightParts: 0,
                    activeConnections: 0,
                    speedBytesPerSec: 0,
                    completingSinceMs: null,
                  }
              : entry,
            ),
          );
          return;
        }
        setQueue((prev) =>
          prev.map((entry) =>
            entry.id === item.id
              ? {
                  ...entry,
                  status: 'error',
                  error: normalizeUiError(err, 'Upload failed'),
                  inFlightParts: 0,
                  activeConnections: 0,
                  speedBytesPerSec: 0,
                  completingSinceMs: null,
                }
                : entry,
            ),
          );
      })
      .finally(() => {
        activeUploadsRef.current.delete(item.id);
        controllersRef.current.delete(item.id);
        setTick((n) => n + 1);
      });
  }, [applyTelemetry]);

  useEffect(() => {
    const availableSlots = maxConcurrentFiles - activeUploadsRef.current.size;
    if (availableSlots <= 0) return;
    const queued = queue.filter((item) => item.status === 'queued').slice(0, availableSlots);
    queued.forEach((item) => startUpload(item));
  }, [maxConcurrentFiles, queue, startUpload, tick]);

  const clearCompleted = useCallback(() => {
    setQueue((prev) =>
      prev.filter((item) => item.status !== 'success' && item.status !== 'error' && item.status !== 'cancelled'),
    );
  }, []);

  const cancelUpload = useCallback((id: string) => {
    const controller = controllersRef.current.get(id);
    if (controller) {
      controller.abort();
      return;
    }
    setQueue((prev) =>
      prev.map((item) =>
        item.id === id && item.status === 'queued'
          ? { ...item, status: 'cancelled' as const, speedBytesPerSec: 0 }
          : item,
      ),
    );
  }, []);

  const retryUpload = useCallback((id: string) => {
    setQueue((prev) =>
      prev.map((item) =>
        item.id === id
          ? {
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
              startedAtMs: null,
              completingSinceMs: null,
              updatedAtMs: null,
              durationMs: null,
            }
          : item,
      ),
    );
    setTick((n) => n + 1);
  }, []);

  const pendingCount = queue.filter(
    (i) => i.status === 'queued' || i.status === 'uploading' || i.status === 'completing',
  ).length;
  const activeCount = queue.filter((i) => i.status === 'uploading' || i.status === 'completing').length;

  const stats = useMemo<UploadStats>(() => {
    const completed = queue.filter((item) => item.status === 'success');
    const originalSize = completed.reduce((sum, item) => sum + item.originalSize, 0);
    return {
      uploaded: completed.length,
      originalSize,
      // The browser only knows logical object bytes; stored size is computed server-side later.
      storedSize: originalSize,
    };
  }, [queue]);

  // All "savings %" math goes through `src/savings.ts` — see that
  // file's header for why we don't roll our own here. The upload
  // batch is conceptually an aggregate over completed objects so it
  // uses the OBJECT view (decimal cap at 99.9), not the scope view
  // (integer cap at 99) that the breadcrumb chip uses.
  const savings = summarizeObjectSavings(stats.originalSize, stats.storedSize).pct;

  return {
    queue,
    stats,
    savings,
    pendingCount,
    activeCount,
    addFiles,
    clearCompleted,
    cancelUpload,
    retryUpload,
  };
}
