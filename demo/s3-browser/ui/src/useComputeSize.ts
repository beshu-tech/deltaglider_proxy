import { useState, useCallback, useRef, useEffect } from 'react';
import { scanPrefixUsage, getPrefixUsage } from './adminApi';
import { getBucket } from './s3client';

export interface FolderSizeState {
  progress: { totalSize: number; totalFiles: number; done: boolean } | null;
  loading: boolean;
  error: string | null;
}

/**
 * Hook to manage "Compute Size" requests for folder prefixes.
 * Uses the background usage scanner API instead of client-side pagination.
 * Also exposes auto-populated sizes from cached scanner results.
 */
export default function useComputeSize() {
  const [sizes, setSizes] = useState<Record<string, FolderSizeState>>({});
  const abortControllers = useRef<Record<string, AbortController>>({});
  const pollTimers = useRef<Record<string, ReturnType<typeof setInterval>>>({});

  // Cleanup poll timers on unmount
  useEffect(() => {
    return () => {
      for (const timer of Object.values(pollTimers.current)) {
        clearInterval(timer);
      }
    };
  }, []);

  const compute = useCallback((prefix: string) => {
    // Cancel any existing computation for this prefix
    if (abortControllers.current[prefix]) {
      abortControllers.current[prefix].abort();
    }
    if (pollTimers.current[prefix]) {
      clearInterval(pollTimers.current[prefix]);
    }

    const controller = new AbortController();
    abortControllers.current[prefix] = controller;

    setSizes((prev) => ({
      ...prev,
      [prefix]: { progress: null, loading: true, error: null },
    }));

    const bucket = getBucket();

    // Trigger the scan
    scanPrefixUsage(bucket, prefix)
      .then(() => {
        if (controller.signal.aborted) return;

        // Poll for results every 2 seconds
        const timer = setInterval(async () => {
          if (controller.signal.aborted) {
            clearInterval(timer);
            return;
          }
          try {
            const result = await getPrefixUsage(bucket, prefix);
            if (controller.signal.aborted) return;
            if (result) {
              clearInterval(timer);
              delete pollTimers.current[prefix];
              setSizes((prev) => ({
                ...prev,
                [prefix]: {
                  progress: {
                    totalSize: result.total_size,
                    totalFiles: result.total_objects,
                    done: true,
                  },
                  loading: false,
                  error: null,
                },
              }));
            }
          } catch {
            // Keep polling on transient errors
          }
        }, 2000);
        pollTimers.current[prefix] = timer;
      })
      .catch((err) => {
        if (controller.signal.aborted) return;
        setSizes((prev) => ({
          ...prev,
          [prefix]: {
            progress: null,
            loading: false,
            error: err instanceof Error ? err.message : String(err),
          },
        }));
      });
  }, []);

  /** Try to auto-populate folder sizes from cached scanner results. */
  const autoPopulate = useCallback(async (currentPrefix: string, folderPrefixes: string[]) => {
    const bucket = getBucket();
    try {
      const result = await getPrefixUsage(bucket, currentPrefix);
      if (!result) return;
      // Populate sizes for folders that have cached data in the children map
      const updates: Record<string, FolderSizeState> = {};
      for (const fp of folderPrefixes) {
        const child = result.children[fp];
        if (child) {
          updates[fp] = {
            progress: { totalSize: child.size, totalFiles: child.objects, done: true },
            loading: false,
            error: null,
          };
        }
      }
      if (Object.keys(updates).length > 0) {
        setSizes((prev) => ({ ...prev, ...updates }));
      }
    } catch {
      // Silently ignore — auto-populate is best-effort
    }
  }, []);

  const cancel = useCallback((prefix: string) => {
    if (abortControllers.current[prefix]) {
      abortControllers.current[prefix].abort();
      delete abortControllers.current[prefix];
    }
    if (pollTimers.current[prefix]) {
      clearInterval(pollTimers.current[prefix]);
      delete pollTimers.current[prefix];
    }
    setSizes((prev) => {
      const next = { ...prev };
      delete next[prefix];
      return next;
    });
  }, []);

  const cancelAll = useCallback(() => {
    for (const key of Object.keys(abortControllers.current)) {
      abortControllers.current[key].abort();
    }
    abortControllers.current = {};
    for (const timer of Object.values(pollTimers.current)) {
      clearInterval(timer);
    }
    pollTimers.current = {};
    setSizes({});
  }, []);

  return { sizes, compute, cancel, cancelAll, autoPopulate };
}
