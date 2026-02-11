import { deleteObject, downloadObject } from '../s3client';
import { formatBytes } from '../utils';
import type { S3Object } from '../types';

interface Props {
  object: S3Object;
  onClose: () => void;
  onDeleted: () => void;
}

/** Headers to skip in the "all headers" section (already shown above or noise) */
const SKIP_HEADERS = new Set([
  'content-length',
  'content-type',
  'etag',
  'last-modified',
  'x-amz-storage-type',
  'x-deltaglider-stored-size',
  'date',
  'vary',
  'access-control-allow-origin',
  'access-control-expose-headers',
]);

function getDgMetadata(headers: Record<string, string>): [string, string][] {
  return Object.entries(headers)
    .filter(([k]) => k.startsWith('x-amz-meta-dg-'))
    .map(([k, v]) => [k.replace('x-amz-meta-dg-', ''), v]);
}

function getUserMetadata(headers: Record<string, string>): [string, string][] {
  return Object.entries(headers)
    .filter(([k]) => k.startsWith('x-amz-meta-') && !k.startsWith('x-amz-meta-dg-'))
    .map(([k, v]) => [k.replace('x-amz-meta-', ''), v]);
}

function getOtherHeaders(headers: Record<string, string>): [string, string][] {
  return Object.entries(headers)
    .filter(([k]) => !SKIP_HEADERS.has(k) && !k.startsWith('x-amz-meta-'))
    .sort(([a], [b]) => a.localeCompare(b));
}

export default function ObjectDetail({ object, onClose, onDeleted }: Props) {
  const savings =
    object.storedSize != null && object.size > 0
      ? ((1 - object.storedSize / object.size) * 100).toFixed(1)
      : null;

  const dgMeta = getDgMetadata(object.headers);
  const userMeta = getUserMetadata(object.headers);
  const otherHeaders = getOtherHeaders(object.headers);

  const handleDelete = async () => {
    await deleteObject(object.key);
    onClose();
    onDeleted();
  };

  const handleDownload = async () => {
    const blob = await downloadObject(object.key);
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = object.key.split('/').pop() || 'download';
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="detail-panel">
      <h3>
        <span>{object.key}</span>
        <button className="btn" onClick={onClose} style={{ padding: '4px 10px' }}>
          Close
        </button>
      </h3>

      <div className="detail-grid">
        <span className="detail-label">Key</span>
        <span className="detail-value">{object.key}</span>

        <span className="detail-label">Original Size</span>
        <span className="detail-value">{formatBytes(object.size)}</span>

        <span className="detail-label">Content-Type</span>
        <span className="detail-value">{object.headers['content-type'] || '--'}</span>

        <span className="detail-label">Storage Type</span>
        <span className="detail-value">
          {object.storageType ? (
            <span className={`badge badge-${object.storageType}`}>
              {object.storageType}
            </span>
          ) : '--'}
        </span>

        <span className="detail-label">Stored Size</span>
        <span className="detail-value">
          {object.storedSize != null ? formatBytes(object.storedSize) : '--'}
        </span>

        <span className="detail-label">Savings</span>
        <span className="detail-value">
          {savings != null ? `${savings}%` : '--'}
        </span>

        <span className="detail-label">ETag</span>
        <span className="detail-value">{object.etag}</span>

        <span className="detail-label">Last Modified</span>
        <span className="detail-value">
          {object.lastModified
            ? new Date(object.lastModified).toLocaleString()
            : '--'}
        </span>
      </div>

      {userMeta.length > 0 && (
        <>
          <h4 className="detail-section-title">User Metadata</h4>
          <div className="detail-grid">
            {userMeta.map(([k, v]) => (
              <span key={k} style={{ display: 'contents' }}>
                <span className="detail-label">{k}</span>
                <span className="detail-value">{v}</span>
              </span>
            ))}
          </div>
        </>
      )}

      {dgMeta.length > 0 && (
        <>
          <h4 className="detail-section-title">DeltaGlider Metadata</h4>
          <div className="detail-grid">
            {dgMeta.map(([k, v]) => (
              <span key={k} style={{ display: 'contents' }}>
                <span className="detail-label">{k}</span>
                <span className="detail-value">{v}</span>
              </span>
            ))}
          </div>
        </>
      )}

      {otherHeaders.length > 0 && (
        <>
          <h4 className="detail-section-title">Response Headers</h4>
          <div className="detail-grid">
            {otherHeaders.map(([k, v]) => (
              <span key={k} style={{ display: 'contents' }}>
                <span className="detail-label">{k}</span>
                <span className="detail-value">{v}</span>
              </span>
            ))}
          </div>
        </>
      )}

      <div style={{ marginTop: 16, display: 'flex', gap: 8 }}>
        <button className="btn btn-primary" onClick={handleDownload}>
          Download
        </button>
        <button className="btn btn-danger" onClick={handleDelete}>
          Delete
        </button>
      </div>
    </div>
  );
}
