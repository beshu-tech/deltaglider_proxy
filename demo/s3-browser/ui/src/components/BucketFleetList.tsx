/**
 * BucketFleetList v2 — THE bucket list. Merges the old fleet view and
 * the Top-buckets table (which showed the same rows twice, side by
 * side, with different decorations) into one full-width list.
 *
 * Each row, on a shared column grid so numerals align down the page:
 *   rank · name + backend chip + status · ratio bar (kept|saved, new
 *   palette) over a footprint under-bar · bytes before→after · saved %
 *   (tier-colored) · age · per-row scan/stop action.
 *
 * Palette story matches the hero: dense teal = kept, luminous green =
 * saved. Rows answer hover with a surface tint and a glow on the
 * saved field.
 */
import { Button } from 'antd';
import { useState } from 'react';
import { PlayCircleOutlined, StopOutlined, ReloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import type { AdminConfig } from '../adminApi';
import { formatBytes, ageLabel } from '../utils';
import { fmtNum } from './dashboard/chartDefaults';
import type { BucketRow } from './AnalyticsSection';

interface Props {
  bucketRows: BucketRow[];
  colors: ReturnType<typeof useColors>;
  config: AdminConfig | null;
  queue: string[];
  scansLoaded: boolean;
  backendOf: (bucket: string) => string | null;
  onStopOne: (bucket: string) => void;
  onScanOne: (bucket: string) => void;
}

/** Tier color for a savings percentage. */
function tierColor(colors: ReturnType<typeof useColors>, pct: number, hasData: boolean): string {
  if (!hasData) return colors.TEXT_FAINT;
  if (pct >= 50) return colors.SAVED_TEXT;
  if (pct >= 20) return colors.KEPT_TEXT;
  if (pct > 0) return colors.TEXT_SECONDARY;
  return colors.ACCENT_AMBER;
}

export default function BucketFleetList({
  bucketRows,
  colors,
  config,
  queue,
  scansLoaded,
  backendOf,
  onStopOne,
  onScanOne,
}: Props) {
  const maxOriginal = Math.max(1, ...bucketRows.map(r => r.totalOriginal));
  return (
    <div
      style={{
        flex: 1,
        minHeight: 0,
        overflow: 'auto',
        display: 'flex',
        flexDirection: 'column',
        justifyContent: bucketRows.length <= 4 ? 'space-evenly' : 'flex-start',
        gap: 4,
      }}
    >
      {bucketRows.map((b, i) => (
        <FleetRow
          key={b.bucket}
          b={b}
          rank={i + 1}
          maxOriginal={maxOriginal}
          colors={colors}
          config={config}
          queue={queue}
          scansLoaded={scansLoaded}
          backendOf={backendOf}
          onStopOne={onStopOne}
          onScanOne={onScanOne}
        />
      ))}
    </div>
  );
}

function FleetRow({
  b,
  rank,
  maxOriginal,
  colors,
  config,
  queue,
  scansLoaded,
  backendOf,
  onStopOne,
  onScanOne,
}: {
  b: BucketRow;
  rank: number;
  maxOriginal: number;
  colors: ReturnType<typeof useColors>;
  config: AdminConfig | null;
  queue: string[];
  scansLoaded: boolean;
  backendOf: (bucket: string) => string | null;
  onStopOne: (bucket: string) => void;
  onScanOne: (bucket: string) => void;
}) {
  const [hover, setHover] = useState(false);
  const hasData = b.totalOriginal > 0;
  const keptPct = hasData ? 100 - b.savingsPercent : 0;
  const footprintPct = (b.totalOriginal / maxOriginal) * 100;
  const pctColor = tierColor(colors, b.savingsPercent, hasData);

  const backendName = backendOf(b.bucket);
  const backendMeta = config?.backends?.find(x => x.name === backendName);
  const backendTip = backendMeta
    ? `${backendMeta.backend_type}${backendMeta.endpoint ? ` · ${backendMeta.endpoint}` : ''}`
    : backendName ?? '';

  // Per-row scan action: stop (live) / dequeue (queued) / scan.
  const isQueuedNonHead = !b.scanning && queue.includes(b.bucket);

  return (
    <div
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        display: 'grid',
        gridTemplateColumns:
          '20px minmax(130px, 0.9fr) minmax(180px, 1.6fr) minmax(150px, auto) 58px 30px',
        alignItems: 'center',
        gap: 14,
        padding: '9px 10px',
        margin: '0 -10px',
        borderRadius: 8,
        background: hover ? `${colors.TEXT_FAINT}14` : 'transparent',
        transition: 'background 120ms ease',
        position: 'relative',
      }}
    >
      {/* rank */}
      <span
        style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 10,
          color: colors.TEXT_FAINT,
          textAlign: 'right',
        }}
      >
        {rank}
      </span>

      {/* name + chips */}
      <div style={{ minWidth: 0 }}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 6,
            fontFamily: 'var(--font-mono)',
            fontSize: 12.5,
            fontWeight: 600,
            color: colors.TEXT_PRIMARY,
            overflow: 'hidden',
            whiteSpace: 'nowrap',
          }}
        >
          <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{b.bucket}</span>
          {b.scanning && (
            <span
              title="Live scan in progress"
              style={{
                width: 6,
                height: 6,
                borderRadius: 6,
                flexShrink: 0,
                background: colors.ACCENT_BLUE,
                animation: 'dgScanPulse 1.2s ease-in-out infinite',
              }}
            />
          )}
          {backendName && (
            <span
              title={backendTip}
              style={{
                fontSize: 9,
                fontWeight: 700,
                color: colors.TEXT_MUTED,
                border: `1px solid ${colors.BORDER}`,
                borderRadius: 4,
                padding: '1px 5px',
                fontFamily: 'var(--font-ui)',
                letterSpacing: '0.04em',
                textTransform: 'lowercase',
                cursor: 'help',
                flexShrink: 0,
              }}
            >
              {backendName}
            </span>
          )}
        </div>
        <div style={{ fontSize: 10.5, color: colors.TEXT_MUTED, marginTop: 2, fontFamily: 'var(--font-ui)' }}>
          {b.scanning && b.objectCount === 0 ? (
            <span style={{ color: colors.ACCENT_BLUE }}>Scanning…</span>
          ) : !b.completedAt && !b.scanning ? (
            <span style={{ color: colors.ACCENT_AMBER }}>not scanned</span>
          ) : (
            <>
              {fmtNum(b.objectCount)} objects
              {b.completedAt && (
                <>
                  {' '}·{' '}
                  <span title={new Date(b.completedAt).toLocaleString()}>
                    {ageLabel(b.completedAt)}
                  </span>
                </>
              )}
            </>
          )}
        </div>
      </div>

      {/* ratio bar + footprint under-bar */}
      <div style={{ display: 'flex', flexDirection: 'column', gap: 3, minWidth: 0 }}>
        <div
          role="img"
          aria-label={
            hasData ? `${b.bucket}: ${b.savingsPercent.toFixed(1)}% saved` : `${b.bucket}: no data`
          }
          style={{
            width: '100%',
            height: 18,
            background: colors.BAR_TRACK,
            border: `1px solid ${colors.BORDER}`,
            borderRadius: 5,
            overflow: 'hidden',
            display: 'flex',
            position: 'relative',
            boxShadow: colors.INSET_SHADOW,
          }}
        >
          {hasData && (
            <>
              <div
                style={{
                  width: `max(${keptPct}%, 2px)`,
                  background: colors.BAR_KEPT,
                  transition: 'width 0.4s cubic-bezier(0.16,1,0.3,1)',
                  flexShrink: 0,
                }}
                title={`Kept: ${formatBytes(b.totalStored)}`}
              />
              <div
                style={{
                  width: `${b.savingsPercent}%`,
                  background: `repeating-linear-gradient(45deg, rgba(255,255,255,0.10) 0 1px, transparent 1px 7px), ${colors.BAR_SAVED}`,
                  boxShadow: hover ? `0 0 14px ${colors.GLOW_GREEN}` : 'none',
                  transition: 'width 0.4s cubic-bezier(0.16,1,0.3,1), box-shadow 0.15s',
                }}
                title={`Saved: ${formatBytes(b.savings)}`}
              />
              {b.savingsPercent > 0 && b.savingsPercent < 100 && (
                <div
                  aria-hidden
                  style={{
                    position: 'absolute',
                    top: 0,
                    bottom: 0,
                    width: 1.5,
                    left: `calc(${keptPct}% - 0.75px)`,
                    background: 'rgba(255,255,255,0.6)',
                    opacity: 0.6,
                  }}
                />
              )}
            </>
          )}
        </div>
        <div
          style={{
            width: '100%',
            height: 4,
            background: colors.BAR_TRACK,
            borderRadius: 2,
            overflow: 'hidden',
            position: 'relative',
          }}
          title={`${formatBytes(b.totalOriginal)} (${footprintPct.toFixed(1)}% of largest)`}
        >
          <div
            style={{
              width: `${Math.max(1, footprintPct)}%`,
              height: '100%',
              background: `linear-gradient(90deg, ${colors.TEXT_FAINT}, ${colors.TEXT_MUTED})`,
              opacity: 0.55,
              borderRadius: 2,
              transition: 'width 0.4s cubic-bezier(0.16,1,0.3,1)',
            }}
          />
        </div>
      </div>

      {/* bytes before → after */}
      <span
        style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 11,
          fontWeight: 500,
          color: colors.TEXT_MUTED,
          fontVariantNumeric: 'tabular-nums',
          textAlign: 'right',
          whiteSpace: 'nowrap',
        }}
      >
        {hasData ? (
          <>
            {formatBytes(b.totalOriginal)}
            <span style={{ opacity: 0.5 }}> → </span>
            <span style={{ color: colors.KEPT_TEXT, fontWeight: 600 }}>
              {formatBytes(b.totalStored)}
            </span>
          </>
        ) : (
          '—'
        )}
      </span>

      {/* saved % */}
      <span
        style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 13,
          fontWeight: 700,
          color: pctColor,
          fontVariantNumeric: 'tabular-nums',
          textAlign: 'right',
        }}
      >
        {hasData ? `${b.savingsPercent.toFixed(1)}%` : '—'}
      </span>

      {/* action */}
      {b.scanning ? (
        <Button
          size="small"
          danger
          icon={<StopOutlined />}
          onClick={() => onStopOne(b.bucket)}
          title={`Stop scanning ${b.bucket}`}
        />
      ) : isQueuedNonHead ? (
        <Button
          size="small"
          type="default"
          icon={<StopOutlined />}
          onClick={() => onStopOne(b.bucket)}
          title={`Remove ${b.bucket} from scan queue`}
        />
      ) : (
        <Button
          size="small"
          type="text"
          icon={b.completedAt ? <ReloadOutlined /> : <PlayCircleOutlined />}
          onClick={() => onScanOne(b.bucket)}
          title={
            b.completedAt
              ? `Re-scan ${b.bucket} (currently ${ageLabel(b.completedAt)})`
              : `Scan ${b.bucket}`
          }
          disabled={!scansLoaded}
        />
      )}
    </div>
  );
}
