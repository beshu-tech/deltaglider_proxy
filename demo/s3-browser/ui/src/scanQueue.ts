/**
 * The "scan these buckets one after another" queue of the dashboard card
 * and the analytics page (review A10, C5). Pure: `useBucketScanQueue` runs
 * the side effects (start, SSE stream, stop).
 *
 * Rules:
 *  - a frame or a lost stream for a bucket that is not the head (a late
 *    frame of a stopped scan) changes nothing;
 *  - a terminal frame (`finished`, or an `error`) moves to the next bucket,
 *    and an error becomes the notice;
 *  - a lost progress stream stops the queue: the scan may still run, and a
 *    second scan must not start beside it;
 *  - a start that fails moves to the next bucket, with a notice.
 */
import type { BucketScanProgress } from './adminApi/bucketScan';

export interface ScanQueueState {
  /** The buckets still to scan; the head is the one that runs. */
  queue: string[];
  /** Why the last scan did not complete, for the operator. */
  notice: string | null;
}

export type ScanQueueEvent =
  | { type: 'replace'; buckets: string[] }
  | { type: 'append'; bucket: string }
  | { type: 'seed'; buckets: string[] }
  | { type: 'frame'; frame: BucketScanProgress }
  | { type: 'streamLost'; bucket: string }
  | { type: 'startFailed'; bucket: string; error: string }
  | { type: 'stop' }
  | { type: 'stopOne'; bucket: string };

export const initialScanQueue: ScanQueueState = { queue: [], notice: null };

export function scanQueueReducer(state: ScanQueueState, event: ScanQueueEvent): ScanQueueState {
  const head = state.queue[0];
  switch (event.type) {
    case 'replace':
      return { queue: [...event.buckets], notice: null };
    case 'append':
      return state.queue.includes(event.bucket)
        ? state
        : { ...state, queue: [...state.queue, event.bucket] };
    case 'seed':
      return state.queue.length > 0 ? state : { ...state, queue: [...event.buckets] };
    case 'frame': {
      const { frame } = event;
      if (frame.bucket !== head || !(frame.finished || frame.error)) return state;
      return {
        queue: state.queue.slice(1),
        notice: frame.error ? `The scan of ${frame.bucket} did not complete: ${frame.error}` : state.notice,
      };
    }
    case 'streamLost':
      if (event.bucket !== head) return state;
      return {
        queue: [],
        notice: `The progress of the scan of ${event.bucket} was lost. The scan can still run; its result shows when it ends.`,
      };
    case 'startFailed':
      if (event.bucket !== head) return state;
      return {
        queue: state.queue.slice(1),
        notice: `The scan of ${event.bucket} did not start: ${event.error}`,
      };
    case 'stop':
      return { queue: [], notice: null };
    case 'stopOne':
      return { ...state, queue: state.queue.filter((b) => b !== event.bucket) };
  }
}
