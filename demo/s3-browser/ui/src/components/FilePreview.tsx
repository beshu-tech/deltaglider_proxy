import { useState, useEffect } from 'react';
import { Modal, Spin, Alert, Button, Typography } from 'antd';
import { DownloadOutlined } from '@ant-design/icons';
import type { S3Object } from '../types';
import { downloadObject, getPresignedUrl } from '../s3client';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

const TEXT_EXTENSIONS = new Set([
  'txt', 'md', 'json', 'yaml', 'yml', 'xml', 'csv', 'log',
  'toml', 'ini', 'cfg', 'conf', 'properties', 'env',
  'sh', 'bash', 'py', 'rs', 'js', 'ts', 'html', 'css',
  'sha', 'sha1', 'sha256', 'sha512', 'sum',
  'gitignore', 'dockerignore', 'dockerfile', 'makefile',
  'license', 'readme', 'changelog',
]);

const IMAGE_EXTENSIONS = new Set([
  'jpg', 'jpeg', 'png', 'gif', 'svg', 'webp', 'bmp', 'ico',
]);

const MAX_TEXT_PREVIEW = 512 * 1024; // 512 KB

export function getPreviewMode(filename: string): 'text' | 'image' | null {
  const ext = filename.split('.').pop()?.toLowerCase() ?? '';
  const basename = filename.split('/').pop()?.toLowerCase() ?? '';
  if (TEXT_EXTENSIONS.has(ext) || TEXT_EXTENSIONS.has(basename)) return 'text';
  if (IMAGE_EXTENSIONS.has(ext)) return 'image';
  return null;
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

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
  const [imageUrl, setImageUrl] = useState('');

  const filename = object?.key.split('/').pop() ?? '';
  const mode = object ? getPreviewMode(object.key) : null;
  const tooLarge = mode === 'text' && (object?.size ?? 0) > MAX_TEXT_PREVIEW;

  useEffect(() => {
    if (!open || !object) return;

    setLoading(true);
    setError('');
    setTextContent('');
    setImageUrl('');

    if (mode === 'text' && !tooLarge) {
      downloadObject(object.key)
        .then(blob => blob.text())
        .then(raw => {
          const ext = object.key.split('.').pop()?.toLowerCase() ?? '';
          if (ext === 'json') {
            try { setTextContent(JSON.stringify(JSON.parse(raw), null, 2)); }
            catch { setTextContent(raw); }
          } else {
            setTextContent(raw);
          }
        })
        .catch(e => setError(e instanceof Error ? e.message : 'Failed to load file'))
        .finally(() => setLoading(false));
    } else if (mode === 'image') {
      getPresignedUrl(object.key)
        .then(url => setImageUrl(url))
        .catch(e => setError(e instanceof Error ? e.message : 'Failed to load image'))
        .finally(() => setLoading(false));
    } else {
      setLoading(false);
    }
  }, [open, object, mode, tooLarge]);

  const handleDownload = async () => {
    if (!object) return;
    try {
      const blob = await downloadObject(object.key);
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = filename;
      a.click();
      URL.revokeObjectURL(url);
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
      width={mode === 'image' ? 720 : 900}
      footer={
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <Text type="secondary" style={{ fontSize: 12 }}>
            {formatSize(object.size)}
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
          message={`File too large for preview (${formatSize(object.size)})`}
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

      {!loading && !error && imageUrl && (
        <div style={{ textAlign: 'center' }}>
          <img
            src={imageUrl}
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
    </Modal>
  );
}
