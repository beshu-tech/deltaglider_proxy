/**
 * platform.ts — lightweight Apple-vs-other detection for rendering
 * the right keyboard symbols in help UIs.
 *
 * The keyboard listener itself always accepts BOTH `metaKey` and
 * `ctrlKey` — that's a backend concern, and cross-platform users
 * (e.g. Mac with external PC keyboard) still get the shortcut. This
 * helper only governs what SYMBOLS to print in the Shortcuts help
 * modal so the operator sees the combo their OS actually documents.
 *
 * Detection priority:
 *   1. `navigator.userAgentData.platform` (modern; Chromium ≥ 90)
 *   2. `navigator.platform` (legacy but still reliable for macOS)
 *   3. `navigator.userAgent` substring fallback for Safari / older
 *      browsers that haven't shipped userAgentData yet
 *
 * Apple covers macOS + iPadOS + iOS. Everything else (Windows,
 * Linux, ChromeOS, Android) gets the Ctrl form.
 */

export function isApplePlatform(): boolean {
  if (typeof navigator === 'undefined') return false;
  // Preferred modern API.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const uaData = (navigator as any).userAgentData as
    | { platform?: string }
    | undefined;
  if (uaData?.platform) {
    return /mac|iphone|ipad|ipod/i.test(uaData.platform);
  }
  // Legacy. `navigator.platform` returns values like "MacIntel",
  // "iPhone", "iPad". Still the most reliable signal on Safari.
  if (navigator.platform) {
    return /mac|iphone|ipad|ipod/i.test(navigator.platform);
  }
  // UA sniffing last resort. Some browsers mask this for privacy;
  // we accept false negatives (Ctrl shown to a Mac user) over
  // false positives (⌘ shown to a Windows user).
  return /mac|iphone|ipad|ipod/i.test(navigator.userAgent || '');
}

/**
 * The meta-key label to display for this platform.
 *
 *   Apple  → "⌘"
 *   other  → "Ctrl"
 *
 * Use for keyboard-shortcut symbols in any help UI. The listener
 * code MUST keep treating `metaKey || ctrlKey` as equivalent —
 * users sometimes plug a PC keyboard into a Mac or vice-versa,
 * and dual-accept is the forgiving choice.
 */
export function metaKeyLabel(): string {
  return isApplePlatform() ? '⌘' : 'Ctrl';
}
