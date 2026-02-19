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
  isTruncated: boolean;
}

export interface BucketInfo {
  name: string;
  creationDate: string;
}
