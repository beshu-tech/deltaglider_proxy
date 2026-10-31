import { useState, useRef, type ReactNode } from 'react';
import { useColors } from '../ThemeContext';
import { useFixedOverlayPosition } from '../useFixedOverlayPosition';
import { BORDER_RADIUS, OVERLAY_SHADOW, Z_INDEX_OVERLAY } from './overlayStyles';

/**
 * Self-contained tooltip. Sibling pattern to SimpleAutoComplete: pure
 * React + `position: fixed` overlay anchored via
 * `getBoundingClientRect`. Bypasses AntD's `<Tooltip>` entirely
 * because this app suppresses all `.ant-tooltip` rendering at the
 * CSS layer (see theme.css; the AntD positioning is broken in
 * the layout we use).
 *
 * Flips above the trigger when there's no room below (via the
 * `overlayEl` opt-in on `useFixedOverlayPosition`).
 *
 * The trigger is whatever the caller wraps. Wrap a single inline
 * element — an icon, a label, a Button — and you'll get a real
 * tooltip on hover and on keyboard focus.
 */
interface Props {
  /** Content rendered inside the tooltip bubble. Plain strings work. */
  hint: ReactNode;
  /** The element(s) that opens the tooltip on hover/focus. */
  children: ReactNode;
  /**
   * Max width of the tooltip bubble in CSS pixels. Long
   * explanations wrap inside this width. Default 280.
   */
  maxWidth?: number;
  /** Tooltip role label for screen readers. Default 'tooltip'. */
  role?: 'tooltip' | 'note';
  /**
   * Inline styles applied to the trigger wrapper `<span>`. Use when
   * the surrounding layout needs the wrapper to participate in flex/
   * grid sizing (e.g. a percentage-width segment of a stack bar).
   * Default styles (`display: inline-flex; cursor: help`) are merged
   * underneath.
   */
  triggerStyle?: React.CSSProperties;
}

export default function HoverHint({ hint, children, maxWidth = 280, role = 'tooltip', triggerStyle }: Props) {
  const colors = useColors();
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLSpanElement>(null);
  const [overlayEl, setOverlayEl] = useState<HTMLDivElement | null>(null);
  const pos = useFixedOverlayPosition(triggerRef, open, { overlayEl, offset: 6 });

  // No-op when hint is empty — render children bare without binding
  // hover handlers. Lets the caller pass null/undefined conditionally
  // without losing focus styling on the trigger.
  if (hint == null || hint === '') {
    return <>{children}</>;
  }

  return (
    <>
      <span
        ref={triggerRef}
        onMouseEnter={() => setOpen(true)}
        onMouseLeave={() => setOpen(false)}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        // The trigger is an inline-flex to keep height matched to its
        // children (icons, text). Cursor `help` signals "info available"
        // on inline triggers; callers that wrap a button can override
        // via the child element's own style. `triggerStyle` lets the
        // caller participate in flex/grid layout (e.g. a percentage-
        // width segment of a stack bar).
        style={{
          display: 'inline-flex',
          alignItems: 'center',
          cursor: 'help',
          ...triggerStyle,
        }}
      >
        {children}
      </span>
      {open && (
        <div
          ref={setOverlayEl}
          role={role}
          style={{
            position: 'fixed',
            top: pos.top,
            left: pos.left,
            maxWidth,
            padding: '8px 12px',
            fontSize: 12,
            lineHeight: 1.5,
            fontFamily: 'var(--font-ui)',
            // Explicit resets — without these, a tooltip wrapping a
            // table header inherits text-transform: uppercase /
            // letter-spacing / font-weight from the <th>, which makes
            // the bubble unreadable. The bubble is its own context.
            textTransform: 'none',
            letterSpacing: 'normal',
            fontWeight: 400,
            color: colors.TEXT_PRIMARY,
            background: colors.BG_ELEVATED,
            border: `1px solid ${colors.BORDER}`,
            borderRadius: BORDER_RADIUS.md,
            boxShadow: OVERLAY_SHADOW,
            zIndex: Z_INDEX_OVERLAY,
            // Bubble shouldn't capture mouse — moving the mouse onto
            // the bubble would `onMouseLeave` the trigger and close it.
            pointerEvents: 'none',
            // Wrap rather than overflow.
            whiteSpace: 'normal',
            wordBreak: 'break-word',
          }}
        >
          {hint}
        </div>
      )}
    </>
  );
}
