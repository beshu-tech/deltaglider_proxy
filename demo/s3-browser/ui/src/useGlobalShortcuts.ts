/**
 * App-wide keyboard shortcuts, mounted once at the App root.
 *
 * These work in EVERY view (browser, docs, metrics, upload, admin) — unlike the
 * admin-only ⌘K/⌘S palette shortcuts that live inside AdminPage. They use the
 * cross-platform command modifier (⌘ on Apple, Ctrl elsewhere; the listener
 * accepts both — see `isCommandCombo`):
 *
 *   ⌘/Ctrl + ,   → open Settings (admin)   [the universal "Preferences" combo]
 *   ⌘/Ctrl + /   → open Docs
 *   ?            → open the shortcuts help modal (suppressed while typing)
 *
 * Built on the shared `useDocumentEvent` listener pattern. Suppressed while the
 * focus is in a text field; `?` is also suppressed when an overlay is open so
 * it doesn't fire underneath an active modal.
 */
import { useDocumentEvent } from './useDocumentEvent';
import { isTypingTarget, isCommandCombo, anyOverlayOpen } from './keyboard';

interface GlobalShortcutHandlers {
  /** Open Settings (admin view). */
  onSettings: () => void;
  /** Open Docs view. */
  onDocs: () => void;
  /** Open the keyboard-shortcuts help modal. */
  onHelp: () => void;
  /** Master gate — listener does nothing while false (e.g. not authenticated). */
  enabled?: boolean;
}

export function useGlobalShortcuts({
  onSettings,
  onDocs,
  onHelp,
  enabled = true,
}: GlobalShortcutHandlers) {
  useDocumentEvent(
    'keydown',
    (e) => {
      // Never hijack typing.
      if (isTypingTarget(e.target)) return;

      if (isCommandCombo(e)) {
        if (e.key === ',') {
          e.preventDefault();
          onSettings();
          return;
        }
        if (e.key === '/') {
          e.preventDefault();
          onDocs();
          return;
        }
        return;
      }

      // `?` (Shift+/ on most layouts) opens help — but not under an open overlay,
      // and not as a modifier combo.
      if (e.key === '?' && !e.metaKey && !e.ctrlKey && !e.altKey) {
        if (anyOverlayOpen()) return;
        e.preventDefault();
        onHelp();
      }
    },
    enabled,
  );
}
