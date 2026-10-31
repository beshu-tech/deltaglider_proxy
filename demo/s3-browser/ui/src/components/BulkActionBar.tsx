import { useState, useCallback } from 'react';
import { Button, Progress, Spin, message } from 'antd';
import { DeleteOutlined, CopyOutlined, ScissorOutlined, DownloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import DestinationPickerModal from './DestinationPickerModal';
import { isSessionExpired } from '../errorHandling';
import { plainError } from '../plainError';
import { SPACE } from './shared-styles';
import { useBackClosesModal } from '../hooks/useOverlayClose';
import { type BulkProgress, bulkProgressPercent, bulkProgressText } from '../bulkBatches';
import { type DeleteJobOutcome, type DeletePlanSummary, bulkDeleteConfirm, deleteJobMessage } from '../bulkDelete';
import { type BulkTransferOutcome, bulkTransferOutcomeMessage } from '../bulkTransfer';
import { confirmDeletePlan } from './confirmDelete';
import { pluralize } from '../utils';
import { useIsNarrow } from '../useIsNarrow';

/** Below this width the action buttons show their icon only (with a title):
 *  "N selected · Copy · Move · ZIP · Delete" did not fit a phone, and the
 *  clipped one was Delete. */
const ICON_ONLY_BELOW = 480;

interface Props {
  selectedCount: number;
  /**
   * Starts the delete: counts the objects, asks `confirm` with the exact
   * numbers, then starts the server job that deletes. Resolves with the
   * outcome, rejects on a failure.
   */
  onDelete?: (confirm: (plan: DeletePlanSummary) => Promise<boolean>) => Promise<DeleteJobOutcome>;
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

export default function BulkActionBar({ selectedCount, onDelete, onCopy, onMove, onDownloadZip, progress, onCancel, hint, currentPrefix, selectionKeys, onSessionExpired }: Props) {
  const colors = useColors();
  // "2 selected objects" / "1 selected object": the names of the bulk buttons.
  const selected = pluralize(selectedCount, 'selected object');
  const [modal, setModal] = useState<'copy' | 'move' | null>(null);
  const [operating, setOperating] = useState(false);
  const [downloading, setDownloading] = useState(false);
  const iconOnly = useIsNarrow(ICON_ONLY_BELOW);
  /** The visible label of an action button: none on a phone (the title says it). */
  const labelOf = (text: string) => (iconOnly ? undefined : text);

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
      else message.error(plainError(e, `The ${op} failed.`).text);
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
      else message.error(plainError(e, 'The download failed.').text);
    } finally {
      setDownloading(false);
    }
  };

  // The bar unmounts when a run ends with an empty selection, so the report
  // goes through the static `message`, never through component state.
  const handleDelete = async () => {
    if (!onDelete) return;
    try {
      // A started job is progress, not a result: an info toast, never the
      // success green (the objects are still being deleted).
      const shown = deleteJobMessage(await onDelete((plan) => confirmDeletePlan(bulkDeleteConfirm(plan))));
      if (shown) message.info(shown.text);
    } catch (e) {
      if (isSessionExpired(e) && onSessionExpired) onSessionExpired();
      else message.error(plainError(e, 'The delete failed.').text);
    }
  };

  const busy = operating || downloading;
  // While the confirmation dialog asks, it is the one surface: a second line
  // here that says "waiting for your confirmation" is noise.
  const shown = progress?.phase === 'confirming' ? null : progress;
  const percent = shown ? bulkProgressPercent(shown) : null;
  const statusStyle = { fontSize: 13, fontFamily: 'var(--font-ui)', color: colors.TEXT_SECONDARY, whiteSpace: 'nowrap' } as const;

  return (
    <>
      {/* Floats over the listing instead of sitting above it: an in-flow bar
          pushed every row down the moment the first box was ticked, so the
          next click landed on a different row. */}
      {progress?.phase !== 'confirming' && (
      <div
        role="toolbar"
        aria-label="Selection actions"
        style={{
          position: 'absolute', left: '50%', bottom: 56, transform: 'translateX(-50%)',
          zIndex: 20, maxWidth: 'calc(100% - 32px)',
          display: 'flex', alignItems: 'center', gap: SPACE.xs,
          padding: iconOnly ? `${SPACE.xs}px ${SPACE.sm}px` : `${SPACE.xs}px ${SPACE.sm}px ${SPACE.xs}px ${SPACE.md}px`,
          border: `1px solid ${colors.BORDER}`, borderRadius: 10,
          background: colors.BG_ELEVATED,
          boxShadow: '0 8px 28px rgba(0, 0, 0, 0.35)',
        }}
      >
        {shown ? (
          // A running bulk action replaces the actions: its phase, a bar and Cancel.
          <>
            <span role="status" style={statusStyle}>
              {bulkProgressText(shown)}
            </span>
            {percent === null ? (
              // Counting: the size of a folder is unknown until it is listed.
              <Spin size="small" aria-label={`Bulk ${shown.action} counting`} />
            ) : (
              <Progress
                aria-label={`Bulk ${shown.action} progress`}
                percent={percent}
                status="active"
                showInfo={false}
                size="small"
                style={{ width: 140, margin: 0 }}
              />
            )}
            <Button
              size="small"
              onClick={onCancel}
              disabled={!onCancel || shown.stopping}
              aria-label={`Cancel ${shown.action}`}
            >
              Cancel
            </Button>
          </>
        ) : (
          <>
            <span style={{ ...statusStyle, marginRight: iconOnly ? 0 : SPACE.xs }}>
              {selectedCount} selected
              {hint ? (
                <span style={{ display: 'block', marginTop: SPACE.xxs, fontSize: 12, color: colors.TEXT_MUTED }}>
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
                aria-label={`Copy ${selected}`}
                title={`Copy ${selected}`}
              >
                {labelOf('Copy')}
              </Button>
            )}
            {onMove && (
              <Button
                size="small"
                icon={<ScissorOutlined />}
                onClick={() => setModal('move')}
                disabled={busy}
                aria-label={`Move ${selected}`}
                title={`Move ${selected}`}
              >
                {labelOf('Move')}
              </Button>
            )}
            {onDownloadZip && (
              <Button
                size="small"
                icon={<DownloadOutlined />}
                onClick={handleZip}
                loading={downloading}
                disabled={busy}
                aria-label={`Download ${selected} as ZIP`}
                title={`Download ${selected} as ZIP`}
              >
                {labelOf('ZIP')}
              </Button>
            )}
            {onDelete && (
              // eslint-disable-next-line no-restricted-syntax -- toolbar action on the selection (confirmed), not a row action
              <Button
                danger
                size="small"
                icon={<DeleteOutlined />}
                // Counts first (nothing is deleted), then asks with the exact number.
                onClick={() => void handleDelete()}
                disabled={busy}
                aria-label={`Delete ${selected}`}
                title={`Delete ${selected}`}
              >
                {labelOf('Delete')}
              </Button>
            )}
          </>
        )}
      </div>
      )}

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
