/**
 * StickyDirtyBar — a slim, bottom-pinned "unsaved changes" action bar.
 *
 * Replaces the bulky in-flow AntD `Alert` that pushed page content down when it
 * appeared (layout shift). This bar is `position: sticky; bottom`, so it floats
 * over the scroll viewport without displacing content, stays reachable while you
 * scroll, and animates in/out subtly. The pattern admin UIs converge on
 * (GitHub / Linear / Vercel settings bars).
 *
 * Renders nothing (and reserves no space) when not dirty — but because it's
 * sticky/floating, its appearance never shifts the layout above it.
 */
import { Button, Space } from 'antd';
import { ExclamationCircleFilled } from '@ant-design/icons';
import { useColors } from '../ThemeContext';

interface Props {
  visible: boolean;
  applying: boolean;
  /** Number of blocking validation errors; Apply is disabled when > 0. */
  errorCount?: number;
  onDiscard: () => void;
  onApply: () => void;
  /**
   * When true, pin to the viewport bottom with `position: fixed` instead of
   * `sticky`. Use this when the bar can't be rendered LAST in the scroll flow
   * (e.g. a shared apply-rail rendered at the top of a panel). Sticky (default)
   * is preferred when the bar IS the last child — it scopes to the scroll
   * container and needs no layout assumptions.
   */
  floating?: boolean;
  /**
   * Inline mode: no positioning at all — the bar renders where it sits
   * (top of its card). Used when SEVERAL independent bars can be visible
   * at once (the System page's stacked cards) and fixed/sticky bars
   * would overlap.
   */
  inline?: boolean;
}

export default function StickyDirtyBar({
  visible,
  applying,
  errorCount = 0,
  onDiscard,
  onApply,
  floating = false,
  inline = false,
}: Props) {
  const c = useColors();
  const hasErrors = errorCount > 0;

  const positionStyle: React.CSSProperties = inline
    ? { height: visible ? 'auto' : 0, marginBottom: visible ? 8 : 0 }
    : floating
      ? {
        // Fixed to the viewport bottom-center. Anchored right of the sidebar
        // isn't worth the complexity — centering reads fine over the content.
        position: 'fixed',
        left: 0,
        right: 0,
        bottom: 16,
      }
    : {
        position: 'sticky',
        bottom: 12,
        height: visible ? 'auto' : 0,
        marginTop: visible ? 8 : 0,
      };

  return (
    <div
      // pointer-events none on the wrapper lets clicks through the empty
      // margins; the inner bar re-enables them.
      style={{
        ...positionStyle,
        // Above the AntD Drawer/Modal (z-index 1000): a rule edited INSIDE the
        // Jobs drawer raises this page-level bar, so it must float OVER the
        // drawer to stay reachable (it sat behind it at z-index 20). Inline mode
        // has no positioning, so the high z is harmless there.
        zIndex: 1001,
        display: 'flex',
        justifyContent: 'center',
        pointerEvents: 'none',
        // Subtle enter/leave: translate + fade, no reflow of siblings.
        opacity: visible ? 1 : 0,
        transform: visible ? 'translateY(0)' : 'translateY(8px)',
        transition: 'opacity 160ms ease, transform 160ms ease',
      }}
      aria-hidden={!visible}
      // A collapsed bar is still mounted (for the fade), and aria-hidden
      // alone left its Discard/Apply buttons in the Tab order: keyboard users
      // landed on invisible buttons on a page with nothing to apply.
      inert={!visible}
    >
      <div
        style={{
          pointerEvents: visible ? 'auto' : 'none',
          display: 'flex',
          alignItems: 'center',
          // Wrap so the message + Discard/Apply don't overflow a phone width.
          flexWrap: 'wrap',
          rowGap: 8,
          gap: 14,
          padding: '8px 12px 8px 14px',
          borderRadius: 12,
          background: c.BG_ELEVATED,
          border: `1px solid ${hasErrors ? c.ACCENT_RED : c.BORDER}`,
          boxShadow: '0 8px 24px rgba(0,0,0,0.18)',
          maxWidth: 'min(560px, calc(100vw - 24px))',
        }}
      >
        <ExclamationCircleFilled
          style={{ color: hasErrors ? c.ACCENT_RED : c.ACCENT_AMBER, fontSize: 15 }}
        />
        <span style={{ fontSize: 13, color: c.TEXT_PRIMARY, whiteSpace: 'nowrap' }}>
          {hasErrors ? (
            <span style={{ color: c.ACCENT_RED }}>
              {errorCount} issue{errorCount === 1 ? '' : 's'} to fix
            </span>
          ) : (
            <>
              Unsaved changes
              <span style={{ color: c.TEXT_MUTED }}> · applies live</span>
            </>
          )}
        </span>
        <Space size={8} style={{ marginLeft: 4 }}>
          <Button size="small" onClick={onDiscard} disabled={applying}>
            Discard
          </Button>
          <Button
            type="primary"
            size="small"
            onClick={onApply}
            disabled={applying || hasErrors}
            loading={applying}
            title={hasErrors ? 'Resolve the issues above before applying' : 'Apply changes'}
          >
            Apply
          </Button>
        </Space>
      </div>
    </div>
  );
}
