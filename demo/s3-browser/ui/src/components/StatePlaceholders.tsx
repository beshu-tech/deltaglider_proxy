/**
 * StatePlaceholders — shared loading + empty-state chrome.
 *
 * Before this existed every panel hand-rolled its own centered <Spin> and
 * empty-state block with slightly different padding (40 / 64 / 32 / 48),
 * icon sizes, and type scales. These two primitives own ONLY the chrome
 * (centering, padding, icon size, title/hint type scale) so the look is
 * identical everywhere; callers still supply their own copy / icon / action
 * so each placeholder stays meaningful in context.
 *
 * Type scale is borrowed from SectionHeader / FormField so an empty state
 * reads as subordinate to a real card header: title at 15px/600 (between
 * SectionHeader's 17 and FormField's 13.5), hint at FormField's 12.5 muted.
 */
import type { ReactNode } from 'react';
import { Skeleton, Spin } from 'antd';
import { useColors } from '../ThemeContext';

/**
 * Centered spinner with consistent padding and an optional label below it.
 * Replaces the ad-hoc `<div style={{ textAlign: 'center', padding: N }}><Spin /></div>`
 * blocks (padding ranged 32–64 across panels; we standardise on 40).
 */
export function LoadingState({ label }: { label?: string }) {
  const { TEXT_MUTED } = useColors();
  return (
    <div
      style={{
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 12,
        padding: 40,
      }}
    >
      <Spin />
      {label && (
        <div
          style={{
            fontSize: 12.5,
            color: TEXT_MUTED,
            fontFamily: 'var(--font-ui)',
            lineHeight: 1.4,
          }}
        >
          {label}
        </div>
      )}
    </div>
  );
}

/**
 * Grey text bars where figures will appear: for a tile or card whose data is
 * still loading. Without it a tile showed its zero / empty copy ("0 buckets
 * ready to scan", "No buckets yet") for a moment before the real figures.
 */
export function SkeletonState({ rows = 3, label = 'Loading' }: { rows?: number; label?: string }) {
  return (
    <div role="status" aria-busy="true" aria-label={label} style={{ flex: 1, padding: 14 }}>
      <Skeleton active title={false} paragraph={{ rows }} />
    </div>
  );
}

/**
 * Centered empty state: optional muted icon, title, optional hint sentence,
 * optional action (an already-rendered AntD <Button>). `compact` is for a
 * small container (a dashboard tile, a table body): less padding, smaller type.
 */
export function EmptyState({
  icon,
  title,
  hint,
  action,
  compact = false,
}: {
  icon?: ReactNode;
  title: ReactNode;
  hint?: ReactNode;
  action?: ReactNode;
  compact?: boolean;
}) {
  const { TEXT_PRIMARY, TEXT_MUTED } = useColors();
  return (
    <div
      style={{
        textAlign: 'center',
        padding: compact ? '16px' : '48px 24px',
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        flex: compact ? 1 : undefined,
      }}
    >
      {icon && (
        <div aria-hidden style={{ fontSize: compact ? 22 : 32, color: TEXT_MUTED, lineHeight: 1, marginBottom: compact ? 8 : 12 }}>
          {icon}
        </div>
      )}
      <div
        style={{
          fontFamily: 'var(--font-ui)',
          fontSize: compact ? 13.5 : 15,
          fontWeight: 600,
          color: TEXT_PRIMARY,
          lineHeight: 1.3,
        }}
      >
        {title}
      </div>
      {hint && (
        <div
          style={{
            fontFamily: 'var(--font-ui)',
            fontSize: 12.5,
            color: TEXT_MUTED,
            lineHeight: 1.45,
            marginTop: compact ? 4 : 6,
            maxWidth: 380,
          }}
        >
          {hint}
        </div>
      )}
      {action && <div style={{ marginTop: compact ? 12 : 16 }}>{action}</div>}
    </div>
  );
}
