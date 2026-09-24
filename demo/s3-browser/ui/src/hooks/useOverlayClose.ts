import { useRef, useCallback, useEffect } from 'react';
import { pushModalEntry, popModalEntryIfTop } from './modalHistory';

/**
 * Direct-load-safe close for overlays (drawers, modals, inspector).
 *
 * `history.back()` only works correctly when the overlay was opened by an
 * in-session `pushState` — the history entry we pushed is on the stack, and
 * Back pops it cleanly. On a direct-loaded or shared deep link, there's no
 * previous entry to go back to (or it's an external page), so we fall back to
 * replacing the URL with the bare path.
 *
 * Usage:
 *   const { markPushed, closeOverlay } = useOverlayClose();
 *   // When opening: navigate(url) then markPushed()
 *   // When closing: closeOverlay(bareUrl, navigate)
 */
export function useOverlayClose() {
  // True when the current overlay URL was pushed by us in this session.
  // False on direct load / shared link, or after Back/Forward.
  const pushedByUs = useRef(false);

  // Reset on popstate — Back/Forward means we're no longer "pushed by us".
  useEffect(() => {
    const onPopState = () => { pushedByUs.current = false; };
    window.addEventListener('popstate', onPopState);
    return () => window.removeEventListener('popstate', onPopState);
  }, []);

  /** Call after navigate(url) when opening an overlay (pushes a history entry). */
  const markPushed = useCallback(() => {
    pushedByUs.current = true;
  }, []);

  /**
   * Close the overlay. If we pushed the entry in-session, `history.back()`
   * pops it. If the overlay was direct-loaded/shared, replace the URL with
   * `bareUrl` (no Back entry to pop).
   */
  const closeOverlay = useCallback(
    (bareUrl: string, navigate: (url: string, opts?: { replace?: boolean }) => void) => {
      if (pushedByUs.current) {
        window.history.back();
      } else {
        navigate(bareUrl, { replace: true });
      }
      pushedByUs.current = false;
    },
    [],
  );

  return { markPushed, closeOverlay };
}

/**
 * Back closes an in-page modal (download/share dialog, destination picker).
 *
 * While `open` is true, one history entry (same URL, tagged state) sits on top
 * of the stack, so Back pops it and calls `onClose` instead of leaving the
 * page. When the modal closes any other way (button, Esc, an object change),
 * the hook pops its own entry — otherwise the next Back would land on the dead
 * entry and appear to do nothing. It pops only when its entry is still the
 * current one, so a navigation pushed on top of it is never undone.
 */
export function useBackClosesModal(open: boolean, onClose: () => void) {
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  });

  useEffect(() => {
    if (!open) return;
    const id = `dg-modal-${Math.random().toString(36).slice(2)}`;
    pushModalEntry(window.history, id, window.location.href);
    let ours = true; // false once Back already popped the entry
    const onPopState = () => {
      ours = false;
      onCloseRef.current();
    };
    window.addEventListener('popstate', onPopState);
    return () => {
      window.removeEventListener('popstate', onPopState);
      if (ours) popModalEntryIfTop(window.history, id);
    };
  }, [open]);
}
