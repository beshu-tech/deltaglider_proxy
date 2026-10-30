export interface S3Object {
  key: string;
  size: number;
  lastModified: string;
  etag: string;
  storageType?: string;
  storedSize?: number;
  /** The listing marks this delta as its folder's baseline (`x-amz-meta-dg-baseline`). */
  isBaseline?: boolean;
  /** All response headers from HEAD, for metadata display */
  headers: Record<string, string>;
}

export interface ListResult {
  objects: S3Object[];
  folders: string[];
  isTruncated: boolean;
  /** The listing stopped with more entries after this key. */
  continuesAfter?: string;
  /** How many LIST pages the listing read. */
  pages?: number;
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
  /** Hover text of the padlock; undefined = plaintext or unknown. */
  encryptedAtRest?: string;
}
