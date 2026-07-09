import { useState, useEffect } from 'react';
import { Modal, Spin, Alert, Button, Typography } from 'antd';
import { DownloadOutlined } from '@ant-design/icons';
import type { S3Object } from '../types';
import { downloadObject, getPresignedUrl } from '../s3client';
import { formatBytes, getFileName, downloadBlobAsFile } from '../utils';
import { useColors } from '../ThemeContext';
import { getPreviewMode } from './filePreviewMode';
import { normalizeUiError } from '../errorHandling';

const { Text } = Typography;

const MAX_TEXT_PREVIEW = 512 * 1024; // 512 KB

interface FilePreviewProps {
  open: boolean;
  object: S3Object | null;
  onClose: () => void;
}

export default function FilePreview({ open, object, onClose }: FilePreviewProps) {
  const colors = useColors();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [textContent, setTextContent] = useState('');
  // Presigned URL for any binary media rendered inline (image / video / audio).
  const [mediaUrl, setMediaUrl] = useState('');

  const filename = object ? getFileName(object.key) : '';
  const mode = object ? getPreviewMode(object.key) : null;
  const tooLarge = mode === 'text' && (object?.size ?? 0) > MAX_TEXT_PREVIEW;

  useEffect(() => {
    if (!open || !object) return;

    // Cancellation guard: if the user switches files (or closes) while a
    // download / presign is in flight, the in-flight result must NOT commit
    // into the new file's drawer state. Without this, A's content used to
    // land inside B's modal after rapid switching.
    let cancelled = false;

    setLoading(true);
    setError('');
    setTextContent('');
    setMediaUrl('');

    if (mode === 'text' && !tooLarge) {
      downloadObject(object.key)
        .then(blob => blob.text())
        .then(raw => {
          if (cancelled) return;
          const ext = object.key.split('.').pop()?.toLowerCase() ?? '';
          if (ext === 'json') {
            try { setTextContent(JSON.stringify(JSON.parse(raw), null, 2)); }
            catch { setTextContent(raw); }
          } else {
            setTextContent(raw);
          }
        })
        .catch(e => { if (!cancelled) setError(normalizeUiError(e, "Failed to load file")); })
        .finally(() => { if (!cancelled) setLoading(false); });
    } else if (mode === 'image' || mode === 'video' || mode === 'audio') {
      // Stream the bytes straight from a presigned URL — the browser's native
      // <img>/<video>/<audio> handles range requests and decoding, so large
      // media never has to be buffered through JS.
      getPresignedUrl(object.key)
        .then(url => { if (!cancelled) setMediaUrl(url); })
        .catch(e => { if (!cancelled) setError(normalizeUiError(e, "Failed to load media")); })
        .finally(() => { if (!cancelled) setLoading(false); });
    } else {
      setLoading(false);
    }

    return () => { cancelled = true; };
    // `mode` and `tooLarge` are pure derivations of `object` (recomputed each
    // render), so they're redundant given `object` — but listed explicitly to
    // keep exhaustive-deps happy and to make the effect's read set self-evident.
  }, [open, object, mode, tooLarge]);

  const handleDownload = async () => {
    if (!object) return;
    try {
      const blob = await downloadObject(object.key);
      downloadBlobAsFile(blob, filename);
    } catch {
      setError('Download failed');
    }
  };

  if (!object) return null;

  return (
    <Modal
      open={open}
      onCancel={onClose}
      title={<Text strong style={{ fontFamily: 'var(--font-mono)', fontSize: 14 }}>{filename}</Text>}
      centered
      width={mode === 'image' || mode === 'video' ? 820 : mode === 'audio' ? 560 : 900}
      footer={
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <Text type="secondary" style={{ fontSize: 12 }}>
            {formatBytes(object.size)}
            {object.headers?.['content-type'] ? ` · ${object.headers['content-type']}` : ''}
          </Text>
          <Button icon={<DownloadOutlined />} onClick={handleDownload}>Download</Button>
        </div>
      }
    >
      {loading && (
        <div style={{ textAlign: 'center', padding: 48 }}><Spin size="large" /></div>
      )}

      {error && (
        <Alert type="error" message={error} showIcon style={{ borderRadius: 8 }} />
      )}

      {!loading && !error && tooLarge && (
        <Alert
          type="info"
          showIcon
          message={`File too large for preview (${formatBytes(object.size)})`}
          description="Text preview is limited to 512 KB. Use the Download button to view the full file."
          style={{ borderRadius: 8 }}
        />
      )}

      {!loading && !error && mode === null && (
        <Alert
          type="info"
          showIcon
          message="Preview not available for this file type"
          description="Use the Download button to save the file locally."
          style={{ borderRadius: 8 }}
        />
      )}

      {!loading && !error && textContent && (
        <pre style={{
          background: colors.BG_BASE,
          border: `1px solid ${colors.BORDER}`,
          borderRadius: 8,
          padding: 16,
          margin: 0,
          maxHeight: 500,
          overflow: 'auto',
          fontFamily: 'var(--font-mono)',
          fontSize: 12,
          lineHeight: 1.6,
          whiteSpace: 'pre-wrap',
          wordBreak: 'break-word',
          color: colors.TEXT_PRIMARY,
        }}>
          {textContent}
        </pre>
      )}

      {!loading && !error && mediaUrl && mode === 'image' && (
        <div style={{ textAlign: 'center' }}>
          <img
            src={mediaUrl}
            alt={filename}
            style={{
              maxWidth: '100%',
              maxHeight: 600,
              borderRadius: 8,
              border: `1px solid ${colors.BORDER}`,
            }}
            onError={() => setError('Failed to load image')}
          />
        </div>
      )}

      {!loading && !error && mediaUrl && mode === 'video' && (
        <div style={{ textAlign: 'center' }}>
          <video
            src={mediaUrl}
            controls
            autoPlay
            playsInline
            style={{
              maxWidth: '100%',
              maxHeight: 600,
              borderRadius: 8,
              border: `1px solid ${colors.BORDER}`,
              background: '#000',
            }}
            onError={() => setError('Failed to load video — the browser may not support this codec. Use Download to play it locally.')}
          />
        </div>
      )}

      {!loading && !error && mediaUrl && mode === 'audio' && (
        <div style={{ textAlign: 'center', padding: '24px 8px' }}>
          <audio
            src={mediaUrl}
            controls
            autoPlay
            style={{ width: '100%' }}
            onError={() => setError('Failed to load audio — the browser may not support this codec. Use Download to play it locally.')}
          />
        </div>
      )}
    </Modal>
  );
}
