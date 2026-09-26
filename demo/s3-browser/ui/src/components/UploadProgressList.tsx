import { memo, useEffect, useMemo, useRef, useState } from 'react';
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
} from '@ant-design/icons';
import { formatBytes } from '../utils';
import type { UploadQueueItem } from '../useUploadQueue';
import { uploadDisplayPath } from '../uploadTelemetry';

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
}

function statusLabel(status: UploadQueueItem['status']): string {
  switch (status) {
    case 'queued':
      return 'Queued';
    case 'uploading':
      return 'Uploading';
    case 'completing':
      return 'Finalizing on backend';
    case 'success':
      return 'Success';
    case 'error':
      return 'Error';
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
}: UploadRowProps) {
  const isCompleting = item.status === 'completing';
  const transferDone = isCompleting || item.status === 'success';
  const showActionCancel = item.status === 'queued' || item.status === 'uploading';
  const showActionRetry = item.status === 'cancelled' || (item.status === 'error' && item.retry !== 'never');
  const displayPath = uploadDisplayPath(item.key, item.destination);
  const completingStartMs = item.completingSinceMs;
  const hasCompletingStart = typeof completingStartMs === 'number' && Number.isFinite(completingStartMs);
  const finalizingElapsedMs = isCompleting && hasCompletingStart
    ? Math.max(0, nowMs - completingStartMs)
    : 0;
  const progressBarWidth = `${Math.max(0, Math.min(100, item.percent))}%`;
  const averageSpeedBytesPerSec = item.durationMs && item.durationMs > 0
    ? item.transferredBytes / (item.durationMs / 1000)
    : 0;
  return (
    <div
      role="listitem"
      aria-label={`${displayPath} — ${item.status}`}
      aria-posinset={index + 1}
      aria-setsize={setSize}
      style={{
        padding: '12px 16px',
        borderBottom: `1px solid ${borderColor}`,
        display: 'grid',
        gap: 8,
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        {statusIcon(item.status, accentBlue, accentGreen, accentRed, borderColor, finalizingColor)}
        {/* Plain CSS ellipsis: the Typography ellipsis measures every
            row, and a queue can hold thousands. */}
        <span
          style={{
            flex: 1,
            minWidth: 0,
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
            fontFamily: 'var(--font-mono)',
            fontSize: 13,
            color: item.status === 'error' ? accentRed : textPrimary,
          }}
          title={item.key}
        >
          {displayPath}
        </span>
        <span
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 11,
            color: isCompleting ? finalizingColor : textMuted,
            border: `1px solid ${isCompleting ? `${finalizingColor}aa` : borderColor}`,
            background: isCompleting ? `${finalizingColor}1f` : 'transparent',
            borderRadius: 999,
            padding: '1px 8px',
            lineHeight: 1.6,
          }}
        >
          {statusLabel(item.status)}
        </span>
        {showActionCancel && (
          <Button
            type="text"
            size="small"
            icon={<StopOutlined />}
            onClick={() => onCancelUpload(item.id)}
            style={{ color: textMuted }}
          >
            Cancel
          </Button>
        )}
        {showActionRetry && (
          <Button
            type="text"
            size="small"
            icon={<ReloadOutlined />}
            onClick={() => onRetryUpload(item.id)}
            style={{ color: textMuted }}
          >
            Retry
          </Button>
        )}
      </div>

      <div
        aria-label="Upload progress"
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
            width: progressBarWidth,
            height: '100%',
            borderRadius: 4,
            background: item.status === 'error'
              ? accentRed
              : item.status === 'success'
                ? accentGreen
                : item.status === 'completing'
                  ? 'repeating-linear-gradient(135deg, #faad14, #faad14 8px, #ffd666 8px, #ffd666 16px)'
                : accentBlue,
            transition: 'width 0.2s ease',
          }}
        />
      </div>

      <div style={{ display: 'flex', flexWrap: 'wrap', gap: 12 }}>
        <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: textMuted }}>
          {formatBytes(item.transferredBytes)} / {formatBytes(item.totalBytes)}
        </Text>
        {transferDone ? (
          <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: textMuted }}>
            Avg speed: {formatSpeed(averageSpeedBytesPerSec)}
          </Text>
        ) : (
          <>
            <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: textMuted }}>
              {formatPercent(item.percent)}
            </Text>
            <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: textMuted }}>
              Speed: {formatSpeed(item.speedBytesPerSec)}
            </Text>
            <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: textMuted }}>
              Parts: {item.completedParts}/{item.totalParts || 0}
              {item.currentPart ? ` (part ${item.currentPart})` : ''}
            </Text>
            <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: textMuted }}>
              Active parts: {item.inFlightParts}
            </Text>
          </>
        )}
        {isCompleting && (
          <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: finalizingColor }}>
            Committing multipart upload • finalizing for {hasCompletingStart ? formatClockMs(finalizingElapsedMs) : '00:00'}
          </Text>
        )}
      </div>

      {item.error && (
        <Text role="alert" style={{ fontSize: 11, color: accentRed }}>
          {item.error}
          {item.retry === 'never' && (
            <span style={{ display: 'block', marginTop: 2 }}>
              Retrying will not help. Fix the cause first, then upload the file again.
            </span>
          )}
          {item.retry === 'after-fix' && (
            <span style={{ display: 'block', marginTop: 2 }}>
              The upload fails again until the bucket has room. Free space or raise the quota, then retry.
            </span>
          )}
        </Text>
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
      style={{ borderRadius: 10, maxHeight, overflowY: 'auto' }}
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
                {...colors}
              />
            </div>
          );
        })}
      </div>
    </div>
  );
}
