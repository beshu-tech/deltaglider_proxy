import { memo, useEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { Button, Typography } from 'antd';
import {
  CheckCircleFilled,
  CloseCircleFilled,
  LoadingOutlined,
  StopOutlined,
  ReloadOutlined,
  MinusCircleFilled,
  ClockCircleOutlined,
  HourglassOutlined,
  CloseOutlined,
} from '@ant-design/icons';
import { formatBytes } from '../utils';
import type { UploadQueueItem } from '../useUploadQueue';
import { uploadDisplayPath } from '../uploadTelemetry';
import TruncatedName from './TruncatedName';
import { plainError } from '../plainError';
import { canRetry, fileState } from '../uploadOutcome';
import { CARD_RADIUS, SPACE } from './shared-styles';

const { Text } = Typography;

interface UploadProgressListProps {
  queue: UploadQueueItem[];
  /** Height at which the list scrolls (default 520 px, about 6 rows). */
  maxHeight?: number;
  borderColor: string;
  textPrimary: string;
  textMuted: string;
  accentBlue: string;
  accentGreen: string;
  accentRed: string;
  finalizingColor: string;
  onCancelUpload: (id: string) => void;
  onRetryUpload: (id: string) => void;
  /** Take a failed file that a retry cannot fix off the list. */
  onRemoveUpload?: (id: string) => void;
}

function statusLabel(status: UploadQueueItem['status']): string {
  switch (status) {
    case 'queued':
      return 'Queued';
    case 'uploading':
      return 'Uploading';
    case 'completing':
      return 'Finalizing';
    case 'success':
      return 'Uploaded';
    case 'error':
      return 'Failed';
    case 'cancelled':
      return 'Cancelled';
    default:
      return status;
  }
}

function statusIcon(
  status: UploadQueueItem['status'],
  accentBlue: string,
  accentGreen: string,
  accentRed: string,
  borderColor: string,
  finalizingColor: string,
) {
  switch (status) {
    case 'success':
      return <CheckCircleFilled style={{ color: accentGreen, fontSize: 16 }} />;
    case 'error':
      return <CloseCircleFilled style={{ color: accentRed, fontSize: 16 }} />;
    case 'cancelled':
      return <MinusCircleFilled style={{ color: borderColor, fontSize: 16 }} />;
    case 'uploading':
      return <LoadingOutlined style={{ color: accentBlue, fontSize: 16 }} />;
    case 'completing':
      return <HourglassOutlined style={{ color: finalizingColor, fontSize: 16 }} />;
    case 'queued':
      return <ClockCircleOutlined style={{ color: borderColor, fontSize: 16 }} />;
    default:
      return null;
  }
}

function formatPercent(value: number): string {
  if (!Number.isFinite(value)) return '0.0%';
  return `${value.toFixed(value >= 100 ? 0 : 1)}%`;
}

function formatSpeed(speedBytesPerSec: number): string {
  if (!Number.isFinite(speedBytesPerSec) || speedBytesPerSec <= 0) return '—';
  return `${formatBytes(speedBytesPerSec)}/s`;
}

function formatClockMs(durationMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(durationMs / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`;
}

type RowColors = Pick<
  UploadProgressListProps,
  'borderColor' | 'textPrimary' | 'textMuted' | 'accentBlue' | 'accentGreen' | 'accentRed' | 'finalizingColor'
>;

interface UploadRowProps extends RowColors {
  item: UploadQueueItem;
  index: number;
  setSize: number;
  /** Wall clock for the "finalizing for" timer; 0 on rows that do not show it. */
  nowMs: number;
  onCancelUpload: (id: string) => void;
  onRetryUpload: (id: string) => void;
  onRemoveUpload?: (id: string) => void;
}

/**
 * One queue row. Memoised: the queue store keeps an unchanged item's
 * identity, so a progress event re-renders only the row it is about.
 */
const UploadRow = memo(function UploadRow({
  item,
  index,
  setSize,
  nowMs,
  borderColor,
  textPrimary,
  textMuted,
  accentBlue,
  accentGreen,
  accentRed,
  finalizingColor,
  onCancelUpload,
  onRetryUpload,
  onRemoveUpload,
}: UploadRowProps) {
  const isCompleting = item.status === 'completing';
  const state = fileState(item.status);
  const running = state === 'uploading';
  const showActionCancel = item.status === 'queued' || item.status === 'uploading';
  // Retry only where it can help (a transient failure, a cancel); a file the
  // proxy refuses every time gets Remove instead (D31).
  const showActionRetry = canRetry(item);
  const showActionRemove = !showActionRetry && state === 'failed' && !!onRemoveUpload;
  const displayPath = uploadDisplayPath(item.key, item.destination);
  const completingStartMs = item.completingSinceMs;
  const hasCompletingStart = typeof completingStartMs === 'number' && Number.isFinite(completingStartMs);
  const finalizingElapsedMs = isCompleting && hasCompletingStart
    ? Math.max(0, nowMs - completingStartMs)
    : 0;
  const percent = Math.max(0, Math.min(100, item.percent));
  const averageSpeedBytesPerSec = item.durationMs && item.durationMs > 0
    ? item.transferredBytes / (item.durationMs / 1000)
    : 0;
  // The plain sentence first; the wire text (status, code, request id) in Details (D24).
  const failure = state === 'failed' ? plainError(item.error, 'The upload failed. Try again.') : null;
  const facts: CSSProperties = { fontFamily: 'var(--font-mono)', fontSize: 12, color: textMuted };
  const words: CSSProperties = { fontFamily: 'var(--font-ui)', fontSize: 12, color: textMuted };
  return (
    <div
      role="listitem"
      aria-label={`${displayPath} — ${statusLabel(item.status).toLowerCase()}`}
      aria-posinset={index + 1}
      aria-setsize={setSize}
      style={{
        padding: `${SPACE.sm}px ${SPACE.md}px`,
        borderBottom: `1px solid ${borderColor}`,
        display: 'grid',
        gap: SPACE.xs,
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs }}>
        {statusIcon(item.status, accentBlue, accentGreen, accentRed, borderColor, finalizingColor)}
        {/* Cut in the middle (no measuring: a queue can hold thousands of
            rows); keys of one batch differ at the end. */}
        <TruncatedName
          text={displayPath}
          title={item.key}
          style={{
            flex: 1,
            fontFamily: 'var(--font-mono)',
            fontSize: 13,
            color: state === 'failed' ? accentRed : textPrimary,
          }}
        />
        <span
          style={{
            fontFamily: 'var(--font-ui)',
            fontSize: 11,
            color: isCompleting ? finalizingColor : textMuted,
            border: `1px solid ${isCompleting ? `${finalizingColor}aa` : borderColor}`,
            background: isCompleting ? `${finalizingColor}1f` : 'transparent',
            borderRadius: 999,
            padding: `1px ${SPACE.xs}px`,
            lineHeight: 1.6,
            flexShrink: 0,
          }}
        >
          {statusLabel(item.status)}
        </span>
        {showActionCancel && (
          <Button type="text" size="small" icon={<StopOutlined />} onClick={() => onCancelUpload(item.id)} aria-label={`Cancel the upload of ${displayPath}`}>
            Cancel
          </Button>
        )}
        {showActionRetry && (
          <Button type="text" size="small" icon={<ReloadOutlined />} onClick={() => onRetryUpload(item.id)} aria-label={`Retry the upload of ${displayPath}`}>
            Retry
          </Button>
        )}
        {showActionRemove && (
          <Button type="text" size="small" icon={<CloseOutlined />} onClick={() => onRemoveUpload?.(item.id)} aria-label={`Remove ${displayPath} from the list`}>
            Remove
          </Button>
        )}
      </div>

      {running && (
        <div
          role="progressbar"
          aria-label={`Upload progress of ${displayPath}`}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(percent)}
          aria-valuetext={isCompleting ? 'Finalizing' : formatPercent(percent)}
          style={{
            width: '100%',
            height: 6,
            borderRadius: 4,
            background: isCompleting ? `${finalizingColor}22` : `${accentBlue}22`,
            overflow: 'hidden',
          }}
        >
          <div
            style={{
              width: `${percent}%`,
              height: '100%',
              borderRadius: 4,
              background: isCompleting
                ? 'repeating-linear-gradient(135deg, #faad14, #faad14 8px, #ffd666 8px, #ffd666 16px)'
                : accentBlue,
              transition: 'width 0.2s ease',
            }}
          />
        </div>
      )}

      {/* Facts in mono, words in the UI face. A failed or cancelled row
          shows no transfer figures: they are all zero (D31). */}
      {running && (
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: SPACE.sm }}>
          <span style={facts}>{formatBytes(item.transferredBytes)} / {formatBytes(item.totalBytes)}</span>
          {!isCompleting && <span style={facts}>{formatPercent(item.percent)}</span>}
          {!isCompleting && item.speedBytesPerSec > 0 && <span style={facts}>{formatSpeed(item.speedBytesPerSec)}</span>}
          {isCompleting && (
            <span style={{ ...words, color: finalizingColor }}>
              The proxy is storing the file · {hasCompletingStart ? formatClockMs(finalizingElapsedMs) : '00:00'}
            </span>
          )}
        </div>
      )}
      {state === 'done' && (
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: SPACE.sm }}>
          <span style={facts}>{formatBytes(item.totalBytes)}</span>
          {averageSpeedBytesPerSec > 0 && <span style={facts}>{formatSpeed(averageSpeedBytesPerSec)}</span>}
        </div>
      )}

      {failure && (
        <Text role="alert" style={{ ...words, color: accentRed }}>
          {failure.text}
        </Text>
      )}

      {/* Diagnostics for a support request, folded away. */}
      {(failure?.details || (running && item.totalParts > 1)) && (
        <details style={words}>
          <summary style={{ cursor: 'pointer', width: 'fit-content' }}>Details</summary>
          {running && item.totalParts > 1 && (
            <div style={facts}>
              Parts {item.completedParts}/{item.totalParts}
              {item.currentPart ? ` (part ${item.currentPart})` : ''} · {item.inFlightParts} in flight
            </div>
          )}
          {failure?.details && (
            <code style={{ ...facts, display: 'block', fontSize: 11, overflowWrap: 'anywhere', whiteSpace: 'pre-wrap' }}>
              {failure.details}
            </code>
          )}
        </details>
      )}
    </div>
  );
});

/** Rows the list renders beyond the visible ones, each side. */
const OVERSCAN = 8;
/** First guess of a row's height; each rendered row is then measured. */
const ESTIMATED_ROW_PX = 84;

export default function UploadProgressList({
  queue,
  maxHeight = 520,
  onCancelUpload,
  onRetryUpload,
  onRemoveUpload,
  ...colors
}: UploadProgressListProps) {
  // Use wall clock to stay aligned with telemetry timestamps even across
  // browser/runtime clock-source differences.
  const [nowMs, setNowMs] = useState(() => Date.now());
  const hasCompleting = useMemo(
    () => queue.some((item) => item.status === 'completing'),
    [queue],
  );

  useEffect(() => {
    if (!hasCompleting) return;
    const id = window.setInterval(() => setNowMs(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [hasCompleting]);

  // Only the rows in view (plus OVERSCAN) are in the DOM: a queue of
  // thousands of files keeps a bounded DOM and render cost.
  const scrollRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: queue.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ESTIMATED_ROW_PX,
    getItemKey: (index) => queue[index].id,
    overscan: OVERSCAN,
    initialRect: { width: 0, height: maxHeight },
  });

  return (
    <div
      ref={scrollRef}
      role="list"
      aria-label="Upload queue"
      className="glass-card"
      style={{ borderRadius: CARD_RADIUS, maxHeight, overflowY: 'auto' }}
    >
      <div style={{ height: virtualizer.getTotalSize(), position: 'relative', width: '100%' }}>
        {virtualizer.getVirtualItems().map((vi) => {
          const item = queue[vi.index];
          return (
            <div
              key={vi.key}
              data-index={vi.index}
              ref={virtualizer.measureElement}
              style={{ position: 'absolute', top: 0, left: 0, width: '100%', transform: `translateY(${vi.start}px)` }}
            >
              <UploadRow
                item={item}
                index={vi.index}
                setSize={queue.length}
                nowMs={item.status === 'completing' ? nowMs : 0}
                onCancelUpload={onCancelUpload}
                onRetryUpload={onRetryUpload}
                onRemoveUpload={onRemoveUpload}
                {...colors}
              />
            </div>
          );
        })}
      </div>
    </div>
  );
}
