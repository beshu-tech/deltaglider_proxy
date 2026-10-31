/**
 * StickyDirtyBar — a slim, bottom-pinned "unsaved changes" action bar.
 *
 * Replaces the bulky in-flow AntD `Alert` that pushed page content down when it
 * appeared (layout shift). This bar is `position: sticky; bottom`, so it floats
 * over the scroll viewport without displacing content, stays reachable while you
 * scroll, and animates in/out subtly. The pattern admin UIs converge on
 * (GitHub / Linear / Vercel settings bars).
 *
 * A hidden bar stays mounted (for the fade) but is `visibility: hidden`, so
 * its buttons are not hit-testable, not focusable and not found by "visible"
 * queries. Before, a page with two bars (Buckets: the page bar and the
 * object-size card's inline bar) had a ghost "Review & apply" over the
 * content, and a click on it opened a 0-change dialog for the wrong section.
 *
 * A floating bar reserves its own height in the flow while it shows, so the
 * last field on the page can scroll out from under it.
 */
import { Button, Space } from 'antd';
import { ExclamationCircleFilled } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { Z_DIRTY_BAR } from '../theme';

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
    ? { height: visible ? 'auto' : 0, marginBottom: visible ? 8 : 0, overflow: visible ? undefined : 'hidden' }
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
        overflow: visible ? undefined : 'hidden',
      };

  const bar = (
    <div
      // pointer-events none on the wrapper lets clicks through the empty
      // margins; the inner bar re-enables them.
      style={{
        ...positionStyle,
        // Above the Drawer, below every Modal (see Z_DIRTY_BAR in theme.ts).
        // Inline mode has no positioning, so the high z is harmless there.
        zIndex: Z_DIRTY_BAR,
        display: 'flex',
        justifyContent: 'center',
        pointerEvents: 'none',
        // Subtle enter/leave: translate + fade, no reflow of siblings.
        opacity: visible ? 1 : 0,
        transform: visible ? 'translateY(0)' : 'translateY(8px)',
        // Hidden only after the fade-out ends; shown at once on the way in.
        visibility: visible ? 'visible' : 'hidden',
        transition: visible
          ? 'opacity 160ms ease, transform 160ms ease'
          : 'opacity 160ms ease, transform 160ms ease, visibility 0s linear 160ms',
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
          gap: 12,
          padding: '8px 12px 8px 12px',
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
            'Unsaved changes'
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
            title={
              hasErrors
                ? 'Resolve the issues above before applying'
                : 'Review the changes, then apply them to the running proxy'
            }
          >
            Review &amp; apply
          </Button>
        </Space>
      </div>
    </div>
  );

  if (!floating || inline) return bar;
  // A fixed bar is out of the flow: reserve its height (bar + bottom offset)
  // at the end of the page while it shows, so it never covers the last card.
  return (
    <>
      {visible && <div data-testid="dirty-bar-spacer" aria-hidden style={{ height: 64, flexShrink: 0 }} />}
      {bar}
    </>
  );
}
