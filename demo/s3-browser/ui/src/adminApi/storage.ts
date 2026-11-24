// Storage status: the disk-capacity answer (src/capacity.rs, GET /storage/status).
import { adminJson } from './core';

interface DiskSpot {
  path: string;
  free_bytes: number;
  total_bytes: number;
}

interface PausedBackend {
  name: string;
  path: string;
  free_bytes: number;
}

/** The admin view: the `state` field tells the shapes apart. */
export type AdminStorageStatus =
  | { state: 'checking' | 'healthy'; largest_file: number }
  | { state: 'filling_up'; largest_file: number; disk: DiskSpot; measured_secs_ago: number | null }
  | { state: 'large_paused'; largest_file: number; disk: DiskSpot | null; measured_secs_ago: number | null }
  | {
      state: 'backend_paused';
      largest_file: number;
      backends: PausedBackend[];
      large_uploads_paused: boolean;
      measured_secs_ago: number | null;
    };

/** Every other session: no path, no backend name, no free-space figure. */
interface BrowserStorageStatus {
  largest_file: number;
  uploads_paused: boolean;
}

export type StorageStatus = AdminStorageStatus | BrowserStorageStatus;

/** Pure: the admin view has a `state`; the browser view has none. */
export function isAdminStatus(status: StorageStatus): status is AdminStorageStatus {
  return 'state' in status;
}

export function getStorageStatus(): Promise<StorageStatus> {
  return adminJson<StorageStatus>('/api/admin/storage/status');
}
