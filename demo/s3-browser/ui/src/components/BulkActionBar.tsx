import { useState, useCallback } from 'react';
import { Button, Progress, message } from 'antd';
import { DeleteOutlined, CopyOutlined, ScissorOutlined, DownloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import DestinationPickerModal from './DestinationPickerModal';
import { isSessionExpired, normalizeUiError } from '../errorHandling';
import { useBackClosesModal } from '../hooks/useOverlayClose';
import { bulkDeleteConfirmText } from '../bulkSelection';
import { type BulkProgress, bulkProgressPercent, bulkProgressText } from '../bulkBatches';
import { type BulkDeleteOutcome, bulkDeleteOutcomeMessage } from '../bulkDelete';
import { type BulkTransferOutcome, bulkTransferOutcomeMessage } from '../bulkTransfer';
import { confirmDelete } from './confirmDelete';

interface Props {
  selectedCount: number;
  /** How many of the selected entries are folders (named in the delete confirm). */
  selectedFolderCount?: number;
  /** Runs the whole delete; resolves with its outcome, rejects on a failure. */
  onDelete?: () => Promise<BulkDeleteOutcome>;
  /** Run the whole copy / move; resolve with its outcome, reject on a failure. */
  onCopy?: (destBucket: string, destPrefix: string) => Promise<BulkTransferOutcome>;
  onMove?: (destBucket: string, destPrefix: string) => Promise<BulkTransferOutcome>;
  onDownloadZip?: () => Promise<void>;
  /** The folder being browsed: the copy/move destination starts there. */
  currentPrefix?: string;
  /** The raw selection keys: the destination picker refuses to copy them onto themselves. */
  selectionKeys?: Iterable<string>;
  /** The running bulk delete, copy or move (null/absent = none): the bar shows it instead of the actions. */
  progress?: BulkProgress | null;
  /** Stops the running bulk action after the batch in flight. */
  onCancel?: () => void;
  /** Shown when bulk handlers are omitted (user signed in for files only). */
  hint?: string;
  /** Called instead of an error toast when the admin session expired. */
  onSessionExpired?: () => void;
}

export default function BulkActionBar({ selectedCount, selectedFolderCount = 0, onDelete, onCopy, onMove, onDownloadZip, progress, onCancel, hint, currentPrefix, selectionKeys, onSessionExpired }: Props) {
  const colors = useColors();
  const [modal, setModal] = useState<'copy' | 'move' | null>(null);
  const [operating, setOperating] = useState(false);
  const [downloading, setDownloading] = useState(false);

  // Back closes the destination picker; closing it any other way pops the entry.
  const closeModal = useCallback(() => setModal(null), []);
  useBackClosesModal(modal !== null, closeModal);

  // The picker closes when the run starts: the bar then shows its progress
  // and Cancel. The report goes through the static `message` (see handleDelete).
  const handleOperation = async (op: 'copy' | 'move', destBucket: string, destPrefix: string) => {
    closeModal();
    const fn = op === 'copy' ? onCopy : onMove;
    if (!fn) return;
    setOperating(true);
    try {
      const { type, text } = bulkTransferOutcomeMessage(await fn(destBucket, destPrefix));
      message[type](text);
    } catch (e) {
      if (isSessionExpired(e) && onSessionExpired) onSessionExpired();
      else message.error(normalizeUiError(e, `${op} failed`));
    } finally {
      setOperating(false);
    }
  };

  const handleZip = async () => {
    setDownloading(true);
    try {
      if (!onDownloadZip) return;
      await onDownloadZip();
    } catch (e) {
      if (isSessionExpired(e) && onSessionExpired) onSessionExpired();
      else message.error(normalizeUiError(e, 'Download failed'));
    } finally {
      setDownloading(false);
    }
  };

  // The bar unmounts when a run ends with an empty selection, so the report
  // goes through the static `message`, never through component state.
  const handleDelete = async () => {
    if (!onDelete) return;
    try {
      const { type, text } = bulkDeleteOutcomeMessage(await onDelete());
      message[type](text);
    } catch (e) {
      if (isSessionExpired(e) && onSessionExpired) onSessionExpired();
      else message.error(normalizeUiError(e, 'Delete failed'));
    }
  };

  const busy = operating || downloading;
  const statusStyle = { fontSize: 13, fontFamily: 'var(--font-ui)', color: colors.TEXT_SECONDARY, whiteSpace: 'nowrap' } as const;

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
        {progress ? (
          // A running bulk action replaces the actions: its phase, a bar and Cancel.
          <>
            <span role="status" style={statusStyle}>
              {bulkProgressText(progress)}
            </span>
            <Progress
              aria-label={`Bulk ${progress.action} progress`}
              percent={bulkProgressPercent(progress)}
              status="active"
              showInfo={false}
              size="small"
              style={{ width: 140, margin: 0 }}
            />
            <Button
              size="small"
              onClick={onCancel}
              disabled={!onCancel || progress.stopping}
              aria-label={`Cancel ${progress.action}`}
            >
              Cancel
            </Button>
          </>
        ) : (
          <>
            <span style={{ ...statusStyle, marginRight: 8 }}>
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
              // eslint-disable-next-line no-restricted-syntax -- toolbar action on the selection (confirmed), not a row action
              <Button
                danger
                size="small"
                icon={<DeleteOutlined />}
                onClick={() =>
                  confirmDelete(bulkDeleteConfirmText(selectedCount, selectedFolderCount), () => void handleDelete())
                }
                disabled={busy}
                aria-label={`Delete ${selectedCount} selected items`}
              >
                Delete
              </Button>
            )}
          </>
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
        selectionKeys={selectionKeys}
      />
    </>
  );
}
