import { useState } from 'react';
import { Button, message } from 'antd';
import { DeleteOutlined, CopyOutlined, ScissorOutlined, DownloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { pluralize } from '../utils';
import DestinationPickerModal from './DestinationPickerModal';
import { normalizeUiError } from '../errorHandling';

interface Props {
  selectedCount: number;
  onDelete?: () => void;
  onCopy?: (destBucket: string, destPrefix: string) => Promise<{ succeeded: number; failed: number }>;
  onMove?: (destBucket: string, destPrefix: string) => Promise<{ succeeded: number; failed: number }>;
  onDownloadZip?: () => Promise<void>;
  deleting: boolean;
  /** Shown when bulk handlers are omitted (user signed in for files only). */
  hint?: string;
}

export default function BulkActionBar({ selectedCount, onDelete, onCopy, onMove, onDownloadZip, deleting, hint }: Props) {
  const colors = useColors();
  const [modal, setModal] = useState<'copy' | 'move' | null>(null);
  const [operating, setOperating] = useState(false);
  const [downloading, setDownloading] = useState(false);

  const handleOperation = async (op: 'copy' | 'move', destBucket: string, destPrefix: string) => {
    setOperating(true);
    try {
      const fn = op === 'copy' ? onCopy : onMove;
      if (!fn) return;
      const result = await fn(destBucket, destPrefix);
      if (result.failed > 0) {
        message.warning(`${result.succeeded} succeeded, ${result.failed} failed`);
      } else {
        message.success(`${pluralize(result.succeeded, 'item')} ${op === 'copy' ? 'copied' : 'moved'}`);
      }
    } catch (e) {
      message.error(normalizeUiError(e, `${op} failed`));
    } finally {
      setOperating(false);
      setModal(null);
    }
  };

  const handleZip = async () => {
    setDownloading(true);
    try {
      if (!onDownloadZip) return;
      await onDownloadZip();
    } catch (e) {
      message.error(normalizeUiError(e, "Download failed"));
    } finally {
      setDownloading(false);
    }
  };

  const busy = deleting || operating || downloading;

  return (
    <>
      <div style={{
        display: 'flex', alignItems: 'center', justifyContent: 'flex-end',
        padding: '8px 20px', gap: 8,
        borderBottom: `1px solid ${colors.BORDER}`,
        background: `${colors.ACCENT_BLUE}08`,
      }}>
        <span style={{ flex: 1, fontSize: 13, fontFamily: 'var(--font-ui)', color: colors.TEXT_SECONDARY }}>
          {selectedCount} selected
          {hint ? (
            <span style={{ display: 'block', marginTop: 4, fontSize: 12, color: colors.TEXT_MUTED }}>
              {hint}
            </span>
          ) : null}
        </span>
        {onCopy && (
          <Button
            size="small"
            icon={<CopyOutlined />}
            onClick={() => setModal('copy')}
            disabled={busy}
            aria-label={`Copy ${selectedCount} selected items`}
          >
            Copy
          </Button>
        )}
        {onMove && (
          <Button
            size="small"
            icon={<ScissorOutlined />}
            onClick={() => setModal('move')}
            disabled={busy}
            aria-label={`Move ${selectedCount} selected items`}
          >
            Move
          </Button>
        )}
        {onDownloadZip && (
          <Button
            size="small"
            icon={<DownloadOutlined />}
            onClick={handleZip}
            loading={downloading}
            disabled={busy}
            aria-label={`Download ${selectedCount} selected items as ZIP`}
          >
            ZIP
          </Button>
        )}
        {onDelete && (
          <Button
            danger
            size="small"
            icon={<DeleteOutlined />}
            onClick={onDelete}
            loading={deleting}
            disabled={busy}
            aria-label={`Delete ${selectedCount} selected items`}
          >
            Delete
          </Button>
        )}
      </div>

      <DestinationPickerModal
        open={modal !== null}
        mode={modal || 'copy'}
        itemCount={selectedCount}
        onConfirm={(bucket, prefix) => { if (modal) handleOperation(modal, bucket, prefix); }}
        onCancel={() => setModal(null)}
        loading={operating}
      />
    </>
  );
}
