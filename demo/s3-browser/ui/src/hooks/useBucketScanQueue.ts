/**
 * Runs the bucket scan queue (`scanQueue.ts`): it starts the head bucket's
 * scan, follows its SSE progress stream, and calls `onSettled` when a scan
 * ends. One source per head, closed by identity, so a late frame or error
 * of a previous bucket never touches the next one.
 */
import { useCallback, useEffect, useReducer, useRef, useState } from 'react';
import { message } from 'antd';
import {
  startBucketScan,
  stopBucketScan,
  subscribeBucketScan,
  type BucketScanProgress,
} from '../adminApi/bucketScan';
import { initialScanQueue, scanQueueReducer } from '../scanQueue';

export function useBucketScanQueue(onSettled: () => void) {
  const [state, dispatch] = useReducer(scanQueueReducer, initialScanQueue);
  const [progress, setProgress] = useState<BucketScanProgress | null>(null);
  const onSettledRef = useRef(onSettled);
  useEffect(() => {
    onSettledRef.current = onSettled;
  }, [onSettled]);
  const head = state.queue[0];

  useEffect(() => {
    if (!head) return;
    let open = true;
    startBucketScan(head).catch((e: unknown) => {
      if (open) {
        dispatch({
          type: 'startFailed',
          bucket: head,
          error: e instanceof Error ? e.message : String(e),
        });
      }
    });
    const close = subscribeBucketScan(
      head,
      (frame) => {
        if (!open || frame.bucket !== head) return;
        if (frame.finished || frame.error) {
          setProgress(null);
          onSettledRef.current();
        } else {
          setProgress(frame);
        }
        dispatch({ type: 'frame', frame });
      },
      () => {
        if (!open) return;
        open = false;
        close();
        setProgress(null);
        dispatch({ type: 'streamLost', bucket: head });
      },
    );
    return () => {
      open = false;
      close();
    };
  }, [head]);

  useEffect(() => {
    if (state.notice) message.warning(state.notice);
  }, [state.notice]);

  const replace = useCallback((buckets: string[]) => dispatch({ type: 'replace', buckets }), []);
  const append = useCallback((bucket: string) => dispatch({ type: 'append', bucket }), []);
  const seed = useCallback((buckets: string[]) => dispatch({ type: 'seed', buckets }), []);
  const stop = useCallback(() => {
    if (head) stopBucketScan(head).catch(() => {});
    setProgress(null);
    dispatch({ type: 'stop' });
  }, [head]);
  const stopOne = useCallback(
    (bucket: string) => {
      if (bucket === head) {
        stopBucketScan(bucket).catch(() => {});
        setProgress(null);
      }
      dispatch({ type: 'stopOne', bucket });
    },
    [head],
  );

  // Progress of the head only: a frame of a bucket that left the queue
  // is never shown.
  const live = progress && progress.bucket === head ? progress : null;
  return { queue: state.queue, live, notice: state.notice, replace, append, seed, stop, stopOne };
}
