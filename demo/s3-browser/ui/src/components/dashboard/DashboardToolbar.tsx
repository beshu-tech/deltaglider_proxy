/**
 * DashboardToolbar — the top strip of the dashboard.
 *
 * One horizontal row:
 *   - Left: optional title (the standalone page only; inside admin settings
 *     the shared page header carries it) + meta (version · backend · uptime).
 *   - Right: labelled control groups — "View" (Monitoring / Analytics) and,
 *     on Monitoring, "Auto-refresh" (off / 5s / 30s) + manual Refresh.
 *
 * Kept visually quiet — this is chrome, not data. No panel borders,
 * no card background, just a horizontal strip.
 */
import type { ReactNode } from 'react';
import { Button } from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import { useColors } from '../../ThemeContext';

export type RefreshCadence = 'off' | '5s' | '30s';

interface Props {
  /** Omitted inside admin settings, where the page header shows the title. */
  title?: string;
  meta?: ReactNode;
  view: 'monitoring' | 'analytics';
  onView: (v: 'monitoring' | 'analytics') => void;
  cadence: RefreshCadence;
  onCadence: (c: RefreshCadence) => void;
  onManualRefresh: () => void;
  loading?: boolean;
}

export default function DashboardToolbar({
  title,
  meta,
  view,
  onView,
  cadence,
  onCadence,
  onManualRefresh,
  loading,
}: Props) {
  const colors = useColors();

  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        gap: 16,
        marginBottom: 14,
        flexWrap: 'wrap',
      }}
    >
      {/* Title (standalone only) + meta */}
      <div style={{ minWidth: 0, flex: '1 1 auto' }}>
        {title && (
          <div
            style={{
              fontSize: 18,
              fontWeight: 700,
              color: colors.TEXT_PRIMARY,
              fontFamily: 'var(--font-ui)',
              letterSpacing: '-0.01em',
              lineHeight: 1.2,
              marginBottom: 2,
            }}
          >
            {title}
          </div>
        )}
        {meta && (
          <div
            style={{
              fontSize: 11.5,
              color: colors.TEXT_MUTED,
              fontFamily: 'var(--font-mono)',
            }}
          >
            {meta}
            {/* LivePulse is the Monitoring-tab "still polling" indicator;
                Analytics doesn't poll so it'd just be a misleading
                static dot. */}
            {view === 'monitoring' && (
              <LivePulse cadence={cadence} loading={loading} colors={colors} />
            )}
          </div>
        )}
      </div>

      <div style={{ display: 'flex', alignItems: 'center', gap: 16, flexWrap: 'wrap' }}>
      <ControlGroup label="View" colors={colors}>
        <Segmented
          value={view}
          options={[
            { value: 'monitoring', label: 'Monitoring' },
            { value: 'analytics', label: 'Analytics' },
          ]}
          onChange={(v) => onView(v as 'monitoring' | 'analytics')}
          colors={colors}
        />
      </ControlGroup>

      {/*
        The refresh controls are Monitoring-tab-only. (The old 5m / 15m / 1h
        range group is gone: 15m and 1h were always disabled, so the group
        offered no choice. The charts' own subtitles name the window.)
        Analytics has its own Re-scan / Stop affordance in the scan-
        status banner (the totals come from a persistent on-disk
        cache, not a refresh poll). Showing the cadence selector on
        Analytics suggested it controlled scan frequency — it
        didn't, it controlled /_/metrics polling.
      */}
      {view === 'monitoring' && (
        <ControlGroup label="Auto-refresh" colors={colors}>
          <Segmented
            value={cadence}
            options={[
              { value: 'off', label: 'Off' },
              { value: '5s', label: '5s' },
              { value: '30s', label: '30s' },
            ]}
            onChange={(v) => onCadence(v as RefreshCadence)}
            colors={colors}
          />
          <Button
            size="small"
            icon={<ReloadOutlined spin={loading} />}
            onClick={onManualRefresh}
            style={{ borderRadius: 8 }}
            title="Refresh now"
            aria-label="Refresh now"
          />
        </ControlGroup>
      )}
      </div>
    </div>
  );
}

/** A small caption in front of a control group, so each group says what it
 *  controls ("View", "Auto-refresh") instead of showing bare values. */
function ControlGroup({
  label,
  colors,
  children,
}: {
  label: string;
  colors: ReturnType<typeof useColors>;
  children: ReactNode;
}) {
  return (
    <div role="group" aria-label={label} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
      <span style={{ fontSize: 12, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>{label}</span>
      {children}
    </div>
  );
}

/**
 * Small pulsing dot next to the title indicating the page is live.
 * Green when auto-refresh is on, amber when paused. Stops animating
 * when `cadence === 'off'`.
 */
function LivePulse({
  cadence,
  loading,
  colors,
}: {
  cadence: RefreshCadence;
  loading?: boolean;
  colors: ReturnType<typeof useColors>;
}) {
  const active = cadence !== 'off';
  const color = active ? colors.ACCENT_GREEN : colors.TEXT_MUTED;
  return (
    <span
      aria-label={active ? `Live, refreshing every ${cadence}` : 'Paused'}
      style={{
        display: 'inline-block',
        width: 7,
        height: 7,
        borderRadius: '50%',
        background: color,
        marginLeft: 10,
        marginBottom: 2,
        verticalAlign: 'middle',
        boxShadow: active ? `0 0 6px ${color}` : 'none',
        opacity: loading ? 0.4 : 1,
        transition: 'opacity 0.2s',
      }}
    />
  );
}

interface SegmentedProps<T extends string> {
  value: T;
  options: { value: T; label: string; disabled?: boolean; hint?: string }[];
  onChange: (v: T) => void;
  colors: ReturnType<typeof useColors>;
}

/**
 * Tiny segmented control. Built from scratch because AntD's
 * `Segmented` drags in an entire theming dance we don't need for a
 * 3-button group. Each option is a button with its own border
 * radius; the active one gets an accent underline.
 */
function Segmented<T extends string>({ value, options, onChange, colors }: SegmentedProps<T>) {
  return (
    <div
      style={{
        display: 'inline-flex',
        background: colors.BG_CARD,
        border: `1px solid ${colors.BORDER}`,
        borderRadius: 8,
        padding: 2,
        gap: 0,
      }}
    >
      {options.map((opt) => {
        const active = opt.value === value;
        return (
          <button
            key={opt.value}
            onClick={() => !opt.disabled && onChange(opt.value)}
            disabled={opt.disabled}
            title={opt.hint}
            style={{
              padding: '4px 10px',
              fontSize: 12,
              fontWeight: 600,
              fontFamily: 'var(--font-ui)',
              border: 'none',
              borderRadius: 6,
              cursor: opt.disabled ? 'not-allowed' : 'pointer',
              background: active ? `${colors.ACCENT_BLUE}22` : 'transparent',
              color: active
                ? colors.ACCENT_BLUE
                : opt.disabled
                  ? colors.TEXT_FAINT
                  : colors.TEXT_SECONDARY,
              opacity: opt.disabled ? 0.6 : 1,
              transition: 'background 0.1s, color 0.1s',
            }}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}
