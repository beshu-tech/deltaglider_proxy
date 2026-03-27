import { useState, useCallback, useRef } from 'react';
import { computePrefixSize } from './s3client';
import type { PrefixSizeProgress } from './s3client';

export interface FolderSizeState {
  progress: PrefixSizeProgress | null;
  loading: boolean;
  error: string | null;
}

/**
 * Hook to manage "Compute Size" requests for folder prefixes.
 * Tracks progress per-folder and supports cancellation.
 */
export default function useComputeSize() {
  const [sizes, setSizes] = useState<Record<string, FolderSizeState>>({});
  const abortControllers = useRef<Record<string, AbortController>>({});

  const compute = useCallback((prefix: string) => {
    // Cancel any existing computation for this prefix
    if (abortControllers.current[prefix]) {
      abortControllers.current[prefix].abort();
    }

    const controller = new AbortController();
    abortControllers.current[prefix] = controller;

    setSizes((prev) => ({
      ...prev,
      [prefix]: { progress: null, loading: true, error: null },
    }));

    computePrefixSize(
      prefix,
      (progress) => {
        // Guard: don't update state if aborted (folder deleted or navigated away)
        if (controller.signal.aborted) return;
        setSizes((prev) => ({
          ...prev,
          [prefix]: { progress, loading: !progress.done, error: null },
        }));
      },
      controller.signal,
    ).catch((err) => {
      if (controller.signal.aborted) return;
      setSizes((prev) => ({
        ...prev,
        [prefix]: {
          progress: prev[prefix]?.progress ?? null,
          loading: false,
          error: err instanceof Error ? err.message : String(err),
        },
      }));
    });
  }, []);

  const cancel = useCallback((prefix: string) => {
    if (abortControllers.current[prefix]) {
      abortControllers.current[prefix].abort();
      delete abortControllers.current[prefix];
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
    setSizes({});
  }, []);

  return { sizes, compute, cancel, cancelAll };
}
