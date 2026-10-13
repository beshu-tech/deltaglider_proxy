/**
 * A clock for relative times ("12s ago", "running for 3m") that moves
 * between data polls. ONE shared 1 s timer runs while any component
 * listens; each component re-renders only when its own period passes, so
 * an old timestamp does not re-render every second.
 */
import { useSyncExternalStore } from 'react';

const listeners = new Set<() => void>();
let timer: ReturnType<typeof setInterval> | null = null;

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  if (!timer) timer = setInterval(() => listeners.forEach((l) => l()), 1000);
  return () => {
    listeners.delete(cb);
    if (listeners.size === 0 && timer) {
      clearInterval(timer);
      timer = null;
    }
  };
}

/**
 * How often a relative time of this age changes its text: every second
 * while it reads in seconds, every 30 s while it reads in minutes, every
 * 5 min after that.
 */
export function tickPeriodMs(ageSec: number): number {
  const a = Math.abs(ageSec);
  if (a < 60) return 1000;
  if (a < 3600) return 30_000;
  return 300_000;
}

/** Current time in ms, rounded down to `periodMs`, refreshed every `periodMs`. */
export function useNow(periodMs = 1000): number {
  return useSyncExternalStore(subscribe, () => Math.floor(Date.now() / periodMs)) * periodMs;
}
