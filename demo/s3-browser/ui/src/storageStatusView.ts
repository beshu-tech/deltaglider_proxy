/**
 * The words of the Storage card and the admin banner, from the capacity
 * status. Pure: the copy is a contract (one sentence when healthy; when not,
 * what is wrong and what to do), so the whole table is unit-tested.
 */
import { formatBytes } from './utils';
import type { AdminStorageStatus } from './adminApi';

/** A size in plain words: "37 GiB", "1.5 GiB" (whole numbers from 10 up). */
export function sizeWords(bytes: number): string {
  const m = /^([\d.]+) (\w+)$/.exec(formatBytes(bytes));
  if (!m) return formatBytes(bytes);
  const n = Number(m[1]);
  return `${n >= 10 ? Math.round(n) : n} ${m[2]}`;
}

export type StorageTone = 'ACCENT_GREEN' | 'ACCENT_BLUE' | 'ACCENT_AMBER' | 'ACCENT_RED';

export interface StorageCardCopy {
  title: string;
  tone: StorageTone;
  /** One sentence per line. */
  lines: string[];
  /** The muted last line, what to do or what happens next. */
  hint?: string;
  /** The "Change the limit" link (healthy only). */
  changeLimit: boolean;
  /** Disk paths that the lines name, so the card can set them in the code face. */
  paths: string[];
}

const RESUME = 'Uploads resume by themselves when space is free.';

export function storageCardCopy(s: AdminStorageStatus): StorageCardCopy {
  const limit = `You can upload files up to ${sizeWords(s.largest_file)}.`;
  switch (s.state) {
    case 'healthy':
      return { title: 'Storage is healthy', tone: 'ACCENT_GREEN', lines: [limit], changeLimit: true, paths: [] };
    case 'checking':
      return { title: 'Checking the disk', tone: 'ACCENT_BLUE', lines: [limit], changeLimit: false, paths: [] };
    case 'filling_up':
      return {
        title: 'The disk is filling up',
        tone: 'ACCENT_AMBER',
        lines: [`Uploads will pause when ${s.disk.path} is full. It has ${sizeWords(s.disk.free_bytes)} left.`],
        hint: 'Free up space on that disk.',
        changeLimit: false,
        paths: [s.disk.path],
      };
    case 'large_paused':
      return {
        title: 'Large uploads are paused',
        tone: 'ACCENT_RED',
        lines: [
          `${s.disk ? `The disk ${s.disk.path}` : 'The disk'} is full. Small files may still upload. Downloads still work.`,
        ],
        hint: RESUME,
        changeLimit: false,
        paths: s.disk ? [s.disk.path] : [],
      };
    case 'backend_paused': {
      const lines = s.backends.map((b) => `The disk ${b.path} is full. Uploads to its buckets are paused. Downloads still work.`);
      return { title: 'Uploads are paused', tone: 'ACCENT_RED', lines, hint: RESUME, changeLimit: false, paths: s.backends.map((b) => b.path) };
    }
  }
}

/** The banner line on every admin page; null when nothing is wrong. */
export function storageBannerCopy(s: AdminStorageStatus): { text: string; tone: StorageTone } | null {
  switch (s.state) {
    case 'filling_up':
      return { text: 'The disk is filling up. Uploads will pause when it is full.', tone: 'ACCENT_AMBER' };
    case 'large_paused':
      return { text: 'Large uploads are paused: the disk is full.', tone: 'ACCENT_RED' };
    case 'backend_paused':
      return { text: 'Uploads are paused: the disk is full.', tone: 'ACCENT_RED' };
    default:
      return null;
  }
}
