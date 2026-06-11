/**
 * Arrow-key navigation for the object browser.
 *
 * Maintains a keyboard "cursor" over the visible rows — folders first, then
 * objects, matching ObjectTable's render order — and translates key presses
 * into the browser's existing navigation primitives (`navigate(prefix)`,
 * `openInspector(key)`). No new product behaviour: every action is something a
 * click already does; the keyboard just reaches it.
 *
 *   ↑ / ↓            move the cursor up / down
 *   Home / End       jump to first / last row
 *   Enter / →        open the cursor row (folder → enter it, object → inspector)
 *   ← / Backspace    go up one folder (to the parent prefix)
 *   Esc              go up one folder
 *
 * Returns the cursor key and a setter so ObjectTable can highlight + scroll the
 * active row and keep the cursor in sync with mouse selection.
 *
 * Gated: does nothing while typing in a field, or while ANY overlay (modal /
 * drawer / open Select) owns the keyboard. The inspector drawer, FilePreview,
 * etc. are overlays, so AntD owns their Esc-to-close; this hook's Esc → "up a
 * folder" only fires when nothing is open. That single `anyOverlayOpen()` gate
 * (applied before every key, Esc included) is what keeps "close the preview"
 * from also navigating the folder underneath it.
 */
import { useCallback, useEffect, useState } from 'react';
import { useDocumentEvent } from './useDocumentEvent';
import { isTypingTarget, anyOverlayOpen } from './keyboard';
import { parentPrefix } from './utils';
import { rowKeysFor, nextCursor } from './browserNav';
import type { S3Object } from './types';

interface BrowserNavArgs {
  /** Folder prefixes at the current level (rendered first). */
  folders: string[];
  /** Objects at the current level (rendered after folders). */
  objects: S3Object[];
  /** Current prefix, for the parent-prefix ("up") computation. */
  prefix: string;
  navigate: (prefix: string) => void;
  openInspector: (key: string) => void;
  /** Master gate — only the browser view enables this. */
  enabled: boolean;
}

interface BrowserNav {
  /** rowKey of the cursor row, or null when nothing is highlighted. */
  cursorKey: string | null;
  /** Sync the cursor (e.g. from a mouse click in ObjectTable). */
  setCursorKey: (key: string | null) => void;
}

export function useBrowserKeyboardNav({
  folders,
  objects,
  prefix,
  navigate,
  openInspector,
  enabled,
}: BrowserNavArgs): BrowserNav {
  const [cursorKey, setCursorKey] = useState<string | null>(null);

  // Drop a stale cursor when the row set changes (folder navigation, search
  // filtering) so we never act on a key that no longer exists.
  useEffect(() => {
    const keys = rowKeysFor(folders, objects);
    if (cursorKey !== null && !keys.includes(cursorKey)) {
      setCursorKey(null);
    }
  }, [folders, objects, cursorKey]);

  const move = useCallback(
    (delta: number | 'first' | 'last') => {
      const keys = rowKeysFor(folders, objects);
      setCursorKey((current) => nextCursor(keys, current, delta));
    },
    [folders, objects],
  );

  const openCursor = useCallback(() => {
    if (cursorKey === null) return;
    if (cursorKey.startsWith('folder:')) {
      navigate(cursorKey.slice('folder:'.length));
    } else {
      openInspector(cursorKey);
    }
  }, [cursorKey, navigate, openInspector]);

  const goUp = useCallback(() => {
    navigate(parentPrefix(prefix));
  }, [navigate, prefix]);

  useDocumentEvent(
    'keydown',
    (e) => {
      // Single gate for EVERY key (Esc included): never hijack typing, and
      // stand down while any overlay owns the keyboard. AntD owns Esc-to-close
      // for the inspector drawer / FilePreview / palette; gating Esc here too
      // is what stops "close the preview" from also navigating the folder
      // underneath it.
      if (isTypingTarget(e.target)) return;
      if (anyOverlayOpen()) return;

      switch (e.key) {
        case 'ArrowDown':
          e.preventDefault();
          move(1);
          break;
        case 'ArrowUp':
          e.preventDefault();
          move(-1);
          break;
        case 'Home':
          e.preventDefault();
          move('first');
          break;
        case 'End':
          e.preventDefault();
          move('last');
          break;
        case 'Enter':
        case 'ArrowRight':
          if (cursorKey !== null) {
            e.preventDefault();
            openCursor();
          }
          break;
        case 'ArrowLeft':
        case 'Backspace':
        case 'Escape':
          if (prefix) {
            e.preventDefault();
            goUp();
          }
          break;
        default:
          break;
      }
    },
    enabled,
  );

  return { cursorKey, setCursorKey };
}
