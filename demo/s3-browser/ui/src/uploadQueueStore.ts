/**
 * Keyed store behind the upload queue (useUploadQueue). React-free so
 * src/__tests__/uploadQueueStore.test.ts can check its cost.
 *
 * Progress events arrive many times per file. Rebuilding the queue array
 * on each event (the old `setQueue(prev => prev.map(...))`) made an
 * upload of n files cost O(n²), plus a render of every row per event.
 * Here an update replaces one entry in a Map (O(1)); `snapshot()` builds
 * the array once per React state update, and only when something changed.
 * Unchanged items keep their identity, so memoised rows skip rendering.
 */
export interface UploadQueueStore<T extends { id: string }> {
  /** Append items, in order. They also join the FIFO of queued work. */
  add(items: T[]): void;
  /** Replace one item with `fn(item)`. Unknown (removed) ids are ignored. */
  update(id: string, fn: (item: T) => T): void;
  get(id: string): T | undefined;
  /** The items in order. The same array while nothing changed. */
  snapshot(): T[];
  /** Pops the FIFO until an item passes `isQueued`. */
  nextQueued(isQueued: (item: T) => boolean): T | undefined;
  /** Put an id at the end of the FIFO again (a retry). */
  requeue(id: string): void;
  remove(pred: (item: T) => boolean): void;
}

export function createUploadQueueStore<T extends { id: string }>(): UploadQueueStore<T> {
  const byId = new Map<string, T>();
  let order: string[] = [];
  let fifo: string[] = [];
  let head = 0;
  let cached: T[] = [];
  let dirty = false;

  return {
    add(items) {
      for (const item of items) {
        byId.set(item.id, item);
        order.push(item.id);
        fifo.push(item.id);
      }
      dirty = true;
    },
    update(id, fn) {
      const item = byId.get(id);
      if (item === undefined) return;
      byId.set(id, fn(item));
      dirty = true;
    },
    get: (id) => byId.get(id),
    snapshot() {
      if (dirty) {
        cached = order.map((id) => byId.get(id)!);
        dirty = false;
      }
      return cached;
    },
    nextQueued(isQueued) {
      while (head < fifo.length) {
        const item = byId.get(fifo[head++]);
        if (item !== undefined && isQueued(item)) return item;
      }
      // Drained: drop the consumed prefix so the FIFO does not grow forever.
      fifo = [];
      head = 0;
      return undefined;
    },
    requeue(id) {
      if (byId.has(id)) fifo.push(id);
    },
    remove(pred) {
      order = order.filter((id) => {
        const keep = !pred(byId.get(id)!);
        if (!keep) byId.delete(id);
        return keep;
      });
      dirty = true;
    },
  };
}
