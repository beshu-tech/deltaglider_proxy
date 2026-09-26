/**
 * Explore finding 7: each upload progress event was one React state
 * update (and a render of the whole queue). Events now reach React
 * batched, and the next file starts without waiting for a render.
 */
import { renderHook, waitFor, act } from '@testing-library/react';
import { expect, test, vi } from 'vitest';

const uploadObject = vi.hoisted(() => vi.fn());
vi.mock('../s3client', () => ({
  getBucket: () => 'releases',
  listDirectObjects: async () => ({ objects: [], prefixes: [] }),
  headObject: async () => ({ headers: {}, storedSize: 1 }),
  uploadObject,
}));

import useUploadQueue from '../useUploadQueue';

const EVENTS_PER_FILE = 5;

test('200 files with 5 progress events each cause far fewer renders than events', async () => {
  // Every event in its own macrotask, like XHR progress events.
  uploadObject.mockImplementation(
    async (key: string, _file: File, opts: { onTelemetry: (t: unknown) => void }) => {
      for (let i = 1; i <= EVENTS_PER_FILE; i++) {
        await new Promise((r) => setTimeout(r, 0));
        opts.onTelemetry({
          key, status: 'uploading', loadedBytes: i, totalBytes: EVENTS_PER_FILE,
          percent: (100 * i) / EVENTS_PER_FILE, speedBytesPerSec: 1, partSize: 1, queueSize: 1,
          totalParts: 1, completedParts: 0, inFlightParts: 1, activeConnections: 1,
          currentPart: 1, elapsedMs: i, updatedAtMs: Date.now(),
        });
      }
    },
  );
  let renders = 0;
  const { result } = renderHook(() => {
    renders++;
    return useUploadQueue('');
  });
  const files = Array.from({ length: 200 }, (_, i) => new File(['x'], `f${i}.txt`));
  act(() => result.current.addFiles(files));
  await waitFor(() => expect(result.current.stats.uploaded).toBe(200), { timeout: 10_000 });
  expect(uploadObject).toHaveBeenCalledTimes(200);
  const events = 200 * EVENTS_PER_FILE;
  expect(renders, `${renders} renders for ${events} events`).toBeLessThan(events / 4);
});
