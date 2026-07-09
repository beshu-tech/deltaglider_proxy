import { useRef, useCallback, useEffect } from 'react';

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
