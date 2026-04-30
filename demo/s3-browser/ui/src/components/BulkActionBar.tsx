import { useState } from 'react';
import { Button, message } from 'antd';
import { DeleteOutlined, CopyOutlined, ScissorOutlined, DownloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import DestinationPickerModal from './DestinationPickerModal';

interface Props {
  selectedCount: number;
  onDelete?: () => void;
  onCopy?: (destBucket: string, destPrefix: string) => Promise<{ succeeded: number; failed: number }>;
  onMove?: (destBucket: string, destPrefix: string) => Promise<{ succeeded: number; failed: number }>;
  onDownloadZip?: () => Promise<void>;
  deleting: boolean;
}

export default function BulkActionBar({ selectedCount, onDelete, onCopy, onMove, onDownloadZip, deleting }: Props) {
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
        message.success(`${result.succeeded} item${result.succeeded !== 1 ? 's' : ''} ${op === 'copy' ? 'copied' : 'moved'}`);
      }
    } catch (e) {
      message.error(e instanceof Error ? e.message : `${op} failed`);
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
      message.error(e instanceof Error ? e.message : 'Download failed');
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
