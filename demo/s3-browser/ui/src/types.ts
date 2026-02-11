export interface S3Object {
  key: string;
  size: number;
  lastModified: string;
  etag: string;
  storageType?: string;
  storedSize?: number;
  /** All response headers from HEAD, for metadata display */
  headers: Record<string, string>;
}

export interface ListResult {
  objects: S3Object[];
  folders: string[];
}

export interface StorageStats {
  total_objects: number;
  total_original_size: number;
  total_stored_size: number;
  savings_percentage: number;
}
