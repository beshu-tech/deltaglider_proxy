import { useEffect, useRef } from 'react';
import { SIDEBAR_MIN_WIDTH } from '../sidebarLayout';
import type { SidebarWidth } from '../useSidebarWidth';

const KEY_STEP = 16;
const KEY_STEP_LARGE = 64;

/** Applied to <body> during a drag: the col-resize cursor everywhere and no
 *  text selection (see theme.css). */
const DRAGGING_CLASS = 'dg-col-resizing';

/**
 * The right edge of the browser sidebar: drag it, or focus it and use the
 * arrow keys (Shift = larger steps, Home/End = narrowest/widest).
 * A double-click restores the default width.
 */
export default function SidebarResizeHandle({ width, maxWidth, resize, save, reset }: SidebarWidth) {
  const drag = useRef<{ startX: number; startWidth: number } | null>(null);
  // An unmount in the middle of a drag must not leave the page unselectable.
  useEffect(() => () => document.body.classList.remove(DRAGGING_CLASS), []);

  const endDrag = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    drag.current = null;
    e.currentTarget.releasePointerCapture(e.pointerId);
    document.body.classList.remove(DRAGGING_CLASS);
    save();
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? KEY_STEP_LARGE : KEY_STEP;
    const next = {
      ArrowLeft: width - step,
      ArrowRight: width + step,
      Home: SIDEBAR_MIN_WIDTH,
      End: maxWidth,
    }[e.key];
    if (next === undefined) return;
    e.preventDefault();
    resize(next);
    save();
  };

  return (
    <div
      className="dg-sidebar-resize-handle"
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize sidebar"
      aria-valuenow={width}
      aria-valuemin={SIDEBAR_MIN_WIDTH}
      aria-valuemax={maxWidth}
      tabIndex={0}
      title="Drag to resize. Double-click to restore the default width."
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        e.currentTarget.setPointerCapture(e.pointerId);
        drag.current = { startX: e.clientX, startWidth: width };
        document.body.classList.add(DRAGGING_CLASS);
      }}
      onPointerMove={(e) => {
        if (drag.current) resize(drag.current.startWidth + e.clientX - drag.current.startX);
      }}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      onDoubleClick={reset}
      onKeyDown={onKeyDown}
    />
  );
}
