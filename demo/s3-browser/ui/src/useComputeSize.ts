import { useState, useCallback, useRef, useEffect } from 'react';
import { scanPrefixUsage, getPrefixUsage } from './adminApi';
import { getBucket } from './s3client';
import { normalizeUiError } from './errorHandling';
import { folderSizeBound, type FolderSizeBound } from './folderSize';
import { USAGE_POLL_INTERVAL_MS, usagePollStep } from './usagePoll';

export interface FolderSizeState {
  /** `bound`: how exact `totalSize` is (see folderSize.ts). */
  progress: { totalSize: number; totalFiles: number; done: boolean; bound?: FolderSizeBound } | null;
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
  const pollTimers = useRef<Record<string, ReturnType<typeof setTimeout>>>({});

  // Cleanup on unmount: stop every poll (abort also detaches any
  // waiting-for-visible listener).
  useEffect(() => {
    const controllers = abortControllers.current;
    const timers = pollTimers.current;
    return () => {
      for (const c of Object.values(controllers)) c.abort();
      for (const timer of Object.values(timers)) clearTimeout(timer);
    };
  }, []);

  const compute = useCallback((prefix: string) => {
    // Cancel any existing computation for this prefix
    if (abortControllers.current[prefix]) {
      abortControllers.current[prefix].abort();
    }
    if (pollTimers.current[prefix]) {
      clearTimeout(pollTimers.current[prefix]);
    }

    const controller = new AbortController();
    abortControllers.current[prefix] = controller;

    setSizes((prev) => ({
      ...prev,
      [prefix]: { progress: null, loading: true, error: null },
    }));

    const bucket = getBucket();
    const settle = (state: FolderSizeState) => {
      delete pollTimers.current[prefix];
      if (abortControllers.current[prefix] === controller) delete abortControllers.current[prefix];
      setSizes((prev) => ({ ...prev, [prefix]: state }));
    };

    // Poll for the result as a setTimeout chain: the next poll is scheduled
    // only after the previous one settles (no overlapping requests), the
    // attempt budget is bounded, non-retryable errors stop it (usagePollStep),
    // and a hidden tab pauses it until the tab is visible again.
    const schedule = (attempt: number) => {
      const run = async () => {
        if (controller.signal.aborted) return;
        if (document.hidden) {
          const onVisible = () => {
            if (document.hidden) return;
            document.removeEventListener('visibilitychange', onVisible);
            void run();
          };
          document.addEventListener('visibilitychange', onVisible);
          controller.signal.addEventListener(
            'abort',
            () => document.removeEventListener('visibilitychange', onVisible),
            { once: true },
          );
          return;
        }
        let outcome: { result: Awaited<ReturnType<typeof getPrefixUsage>> } | { error: unknown };
        try {
          outcome = { result: await getPrefixUsage(bucket, prefix) };
        } catch (error) {
          outcome = { error };
        }
        if (controller.signal.aborted) return;
        // The bucket changed under us: never show A's size under B.
        if (getBucket() !== bucket) return;
        const step = usagePollStep(attempt, outcome);
        if (step.kind === 'retry') {
          schedule(attempt + 1);
        } else if (step.kind === 'fail') {
          settle({ progress: null, loading: false, error: step.error });
        } else if ('result' in outcome && outcome.result) {
          settle({
            progress: {
              totalSize: outcome.result.total_size,
              totalFiles: outcome.result.total_objects,
              done: true,
              bound: folderSizeBound(outcome.result),
            },
            loading: false,
            error: null,
          });
        }
      };
      pollTimers.current[prefix] = setTimeout(() => void run(), USAGE_POLL_INTERVAL_MS);
    };

    // Trigger the scan
    scanPrefixUsage(bucket, prefix)
      .then(() => {
        if (controller.signal.aborted) return;
        schedule(1);
      })
      .catch((err) => {
        if (controller.signal.aborted) return;
        settle({
          progress: null,
          loading: false,
          error: normalizeUiError(err, "Compute size failed"),
        });
      });
  }, []);

  /** Try to auto-populate folder sizes from cached scanner results. */
  const autoPopulate = useCallback(async (currentPrefix: string, folderPrefixes: string[]) => {
    const bucket = getBucket();
    try {
      const result = await getPrefixUsage(bucket, currentPrefix);
      // Drop a result for a bucket the user already left (sizes are keyed by
      // folder prefix only, so it would land under the new bucket's folders).
      if (!result || getBucket() !== bucket) return;
      // Populate sizes for folders that have cached data in the children map
      const updates: Record<string, FolderSizeState> = {};
      for (const fp of folderPrefixes) {
        const child = result.children[fp];
        if (child) {
          updates[fp] = {
            progress: {
              totalSize: child.size,
              totalFiles: child.objects,
              done: true,
              bound: folderSizeBound(result, child),
            },
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
      clearTimeout(pollTimers.current[prefix]);
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
      clearTimeout(timer);
    }
    pollTimers.current = {};
    setSizes({});
  }, []);

  return { sizes, compute, cancel, cancelAll, autoPopulate };
}
