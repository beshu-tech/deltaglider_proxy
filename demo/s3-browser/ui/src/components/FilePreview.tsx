import { useState, useEffect } from 'react';
import { Modal, Alert, Button, Typography, message } from 'antd';
import { DownloadOutlined, FileExclamationOutlined } from '@ant-design/icons';
import type { S3Object } from '../types';
import { downloadObject, getDownloadUrl, getPresignedUrl } from '../s3client';
import { formatBytes, getFileName, downloadFromUrl } from '../utils';
import { useColors } from '../ThemeContext';
import { getPreviewMode } from './filePreviewMode';
import { plainError } from '../plainError';
import { EmptyState, ErrorState, LoadingState } from './StatePlaceholders';
import { SPACE } from './shared-styles';

const { Text } = Typography;

const MAX_TEXT_PREVIEW = 512 * 1024; // 512 KiB

type MediaMode = 'image' | 'video' | 'audio';

/** What the preview says when the browser fetched the file but cannot decode it. */
const UNREADABLE: Record<MediaMode, { title: string; hint: string }> = {
  image: { title: 'This file is not a readable image.', hint: 'Your browser cannot show it. Download the file to inspect it.' },
  video: { title: 'This browser cannot play this video.', hint: 'The codec may not be supported. Download the file to play it locally.' },
  audio: { title: 'This browser cannot play this audio file.', hint: 'The codec may not be supported. Download the file to play it locally.' },
};

interface FilePreviewProps {
  open: boolean;
  object: S3Object | null;
  onClose: () => void;
}

export default function FilePreview({ open, object, onClose }: FilePreviewProps) {
  const colors = useColors();
  const [loading, setLoading] = useState(false);
  // The failure as thrown: ErrorState words it (plainError), raw text in Details.
  const [error, setError] = useState<unknown>(null);
  // Bumped by Retry: the effect loads the file again.
  const [attempt, setAttempt] = useState(0);
  const [textContent, setTextContent] = useState('');
  // Presigned URL for any binary media rendered inline (image / video / audio).
  const [mediaUrl, setMediaUrl] = useState('');
  // The browser loaded the media URL but could not decode it (not a failed request).
  const [unreadable, setUnreadable] = useState<MediaMode | null>(null);

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
    setError(null);
    setTextContent('');
    setMediaUrl('');
    setUnreadable(null);

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
        .catch(e => { if (!cancelled) setError(e ?? new Error("The file could not be read.")); })
        .finally(() => { if (!cancelled) setLoading(false); });
    } else if (mode === 'image' || mode === 'video' || mode === 'audio') {
      // Stream the bytes straight from a presigned URL — the browser's native
      // <img>/<video>/<audio> handles range requests and decoding, so large
      // media never has to be buffered through JS.
      getPresignedUrl(object.key)
        .then(url => { if (!cancelled) setMediaUrl(url); })
        .catch(e => { if (!cancelled) setError(e ?? new Error("The file could not be read.")); })
        .finally(() => { if (!cancelled) setLoading(false); });
    } else {
      setLoading(false);
    }

    return () => { cancelled = true; };
    // `mode` and `tooLarge` are pure derivations of `object` (recomputed each
    // render), so they're redundant given `object` — but listed explicitly to
    // keep exhaustive-deps happy and to make the effect's read set self-evident.
  }, [open, object, mode, tooLarge, attempt]);

  // Native, streamed download (see InspectorPanel): no page-memory Blob.
  const handleDownload = async () => {
    if (!object) return;
    try {
      downloadFromUrl(await getDownloadUrl(object.key, filename), filename);
    } catch (e) {
      // A failed download is not a failed preview: the preview stays.
      void message.error(plainError(e, 'The download could not start. Try again.').text);
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
      {loading && <LoadingState variant="inline" label="Loading the preview" />}

      {error != null && (
        <ErrorState error={error} what="the preview" onRetry={() => setAttempt((n) => n + 1)} />
      )}

      {!loading && error == null && tooLarge && (
        <Alert
          type="info"
          showIcon
          title={`File too large for preview (${formatBytes(object.size)})`}
          description="Text preview is limited to 512 KiB. Use the Download button to view the full file."
          style={{ borderRadius: 8 }}
        />
      )}

      {!loading && error == null && mode === null && (
        <Alert
          type="info"
          showIcon
          title="Preview not available for this file type"
          description="Use the Download button to save the file locally."
          style={{ borderRadius: 8 }}
        />
      )}

      {!loading && error == null && textContent && (
        <pre style={{
          background: colors.BG_BASE,
          border: `1px solid ${colors.BORDER}`,
          borderRadius: 8,
          padding: SPACE.md,
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

      {unreadable && (
        <EmptyState
          icon={<FileExclamationOutlined />}
          title={UNREADABLE[unreadable].title}
          hint={UNREADABLE[unreadable].hint}
          action={<Button type="primary" icon={<DownloadOutlined />} onClick={handleDownload}>Download</Button>}
        />
      )}

      {!loading && error == null && !unreadable && mediaUrl && mode === 'image' && (
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
            onError={() => setUnreadable('image')}
          />
        </div>
      )}

      {!loading && error == null && !unreadable && mediaUrl && mode === 'video' && (
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
            onError={() => setUnreadable('video')}
          />
        </div>
      )}

      {!loading && error == null && !unreadable && mediaUrl && mode === 'audio' && (
        <div style={{ textAlign: 'center', padding: `${SPACE.lg}px ${SPACE.xs}px` }}>
          <audio
            src={mediaUrl}
            controls
            autoPlay
            style={{ width: '100%' }}
            onError={() => setUnreadable('audio')}
          />
        </div>
      )}
    </Modal>
  );
}
