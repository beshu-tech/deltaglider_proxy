import { useState, useCallback } from 'react';
import { Button, Modal, message } from 'antd';
import { DeleteOutlined, CopyOutlined, ScissorOutlined, DownloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { pluralize } from '../utils';
import DestinationPickerModal from './DestinationPickerModal';
import { normalizeUiError } from '../errorHandling';
import { useBackClosesModal } from '../hooks/useOverlayClose';
import { bulkDeleteConfirmText } from '../bulkSelection';

interface Props {
  selectedCount: number;
  /** How many of the selected entries are folders (named in the delete confirm). */
  selectedFolderCount?: number;
  onDelete?: () => void;
  onCopy?: (destBucket: string, destPrefix: string) => Promise<{ succeeded: number; failed: number }>;
  onMove?: (destBucket: string, destPrefix: string) => Promise<{ succeeded: number; failed: number }>;
  onDownloadZip?: () => Promise<void>;
  /** The folder being browsed: the copy/move destination starts there. */
  currentPrefix?: string;
  deleting: boolean;
  /** Shown when bulk handlers are omitted (user signed in for files only). */
  hint?: string;
}

export default function BulkActionBar({ selectedCount, selectedFolderCount = 0, onDelete, onCopy, onMove, onDownloadZip, deleting, hint, currentPrefix }: Props) {
  const colors = useColors();
  const [modal, setModal] = useState<'copy' | 'move' | null>(null);
  const [operating, setOperating] = useState(false);
  const [downloading, setDownloading] = useState(false);

  // Back closes the destination picker; closing it any other way pops the entry.
  const closeModal = useCallback(() => setModal(null), []);
  useBackClosesModal(modal !== null, closeModal);

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
      closeModal();
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
      {/* Floats over the listing instead of sitting above it: an in-flow bar
          pushed every row down the moment the first box was ticked, so the
          next click landed on a different row. */}
      <div
        role="toolbar"
        aria-label="Selection actions"
        style={{
          position: 'absolute', left: '50%', bottom: 56, transform: 'translateX(-50%)',
          zIndex: 20, maxWidth: 'calc(100% - 32px)',
          display: 'flex', alignItems: 'center', gap: 8,
          padding: '8px 12px 8px 16px',
          border: `1px solid ${colors.BORDER}`, borderRadius: 10,
          background: colors.BG_ELEVATED,
          boxShadow: '0 8px 28px rgba(0, 0, 0, 0.35)',
        }}
      >
        <span style={{ marginRight: 8, fontSize: 13, fontFamily: 'var(--font-ui)', color: colors.TEXT_SECONDARY, whiteSpace: 'nowrap' }}>
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
            onClick={() =>
              Modal.confirm({
                title: 'Delete permanently?',
                content: bulkDeleteConfirmText(selectedCount, selectedFolderCount),
                okText: 'Delete',
                okButtonProps: { danger: true },
                onOk: () => onDelete?.(),
              })
            }
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
        onCancel={closeModal}
        loading={operating}
        currentPrefix={currentPrefix}
      />
    </>
  );
}
