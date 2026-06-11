/**
 * Shared keyboard helpers for the app-wide shortcut system.
 *
 * Two concerns live here, both pure (testable without a DOM render):
 *  - `isTypingTarget` — should a global shortcut be SUPPRESSED because the
 *    event originates from a text field / editable element? A literal "?"
 *    typed into a password field must never open the help modal.
 *  - `anyOverlayOpen` — is an AntD modal / drawer / dropdown currently open?
 *    When one is, the overlay owns the keyboard (Esc to close, arrows inside
 *    a Select, etc.), so the browser-navigation arrows must stand down.
 *
 * The established codebase pattern (AdminPage's inline listener) hand-rolls an
 * `inText` check; this centralises it so every global handler agrees on what
 * "the user is typing" and "an overlay is up" mean.
 */

/** Tags whose focus means the user is entering text — suppress global keys. */
const EDITABLE_TAGS = new Set(['INPUT', 'TEXTAREA', 'SELECT']);

/**
 * True when the keyboard event targets a text-entry context (input, textarea,
 * select, or any contenteditable subtree). Global single-key shortcuts (`?`,
 * browser arrows) check this and bail so typing is never hijacked.
 */
export function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (EDITABLE_TAGS.has(target.tagName)) return true;
  return target.isContentEditable;
}

/**
 * True when any AntD overlay (modal, drawer, or open dropdown/select popup) is
 * mounted and visible. Those own the keyboard while open, so browser-level
 * arrow navigation must not also react. We probe the DOM rather than thread
 * open-state through props because overlays are spread across many components
 * (CommandPalette, ShortcutsHelp, InspectorPanel drawer, Select popups, …) and
 * a single DOM query is both simpler and always correct.
 *
 * AntD hides closed popups with `.ant-select-dropdown-hidden` / `.ant-slide-*-leave`
 * but keeps them mounted, so we explicitly exclude the hidden-dropdown class.
 */
export function anyOverlayOpen(doc: Document = document): boolean {
  if (doc.querySelector('.ant-modal-root .ant-modal') !== null) return true;
  if (doc.querySelector('.ant-drawer-open') !== null) return true;
  if (doc.querySelector('.ant-select-dropdown:not(.ant-select-dropdown-hidden)') !== null) {
    return true;
  }
  return false;
}

/**
 * True when the event carries the platform's primary command modifier. We
 * accept BOTH ⌘ (metaKey) and Ctrl regardless of platform so a Mac user on a
 * PC keyboard (or vice-versa) still triggers the shortcut — only the DISPLAYED
 * glyph is platform-specific (see `metaKeyLabel` in platform.ts). `bare` means
 * no Shift/Alt, so we don't hijack ⌘⇧K (DevTools) or ⌘⌥… combos.
 */
export function isCommandCombo(
  e: Pick<KeyboardEvent, 'metaKey' | 'ctrlKey' | 'shiftKey' | 'altKey'>,
): boolean {
  return (e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey;
}
