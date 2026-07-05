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
  backend?: BucketBackendOrigin;
  /** Verbatim backend error when the bucket's backend is unreachable; the
   *  bucket is still listed but flagged unavailable. Undefined = reachable. */
  unavailable?: string;
}

export interface BucketBackendOrigin {
  backendName?: string;
  backendType?: string;
  backendEndpoint?: string;
  backendRegion?: string;
  backendPath?: string;
  realBucket?: string;
}
