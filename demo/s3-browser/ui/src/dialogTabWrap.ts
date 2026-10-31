/**
 * Tab wraps inside an open modal dialog (UX harvest D4).
 *
 * AntD 6 traps focus with `lockFocus` (@rc-component/util): it has no
 * sentinels, it only pulls focus back on `focusin`. A dialog is portalled
 * to the END of <body>, so Tab on its last control leaves the document:
 * focus goes to the browser chrome (`document.activeElement` = BODY) and
 * one Tab press "disappears" before focus comes back to the close ×.
 * This listener moves focus from the last control to the first (and back
 * with Shift+Tab) itself, so the cycle never leaves the dialog.
 *
 * Only a dialog that traps focus wraps: `aria-modal="true"`, and for an
 * AntD modal or drawer, one that shows its mask (a drawer with
 * `mask={false}` leaves the page usable, so Tab may leave it).
 */

const TABBABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  'summary',
  '[tabindex]',
  '[contenteditable="true"]',
].join(',');

function isShown(el: Element): boolean {
  for (let n: Element | null = el; n; n = n.parentElement) {
    const cs = getComputedStyle(n);
    if (cs.display === 'none' || cs.visibility === 'hidden') return false;
    if (n.hasAttribute('hidden') || n.getAttribute('aria-hidden') === 'true') return false;
  }
  return true;
}

function trapsFocus(dialog: Element): boolean {
  const antRoot = dialog.closest('.ant-modal-root, .ant-drawer');
  if (!antRoot) return true;
  const mask = antRoot.querySelector(':scope > .ant-modal-mask, :scope > .ant-drawer-mask');
  return !!mask && isShown(mask);
}

/** Pure DOM query: the open, focus-trapping dialog on top, or null. */
export function topmostModalDialog(doc: Document): HTMLElement | null {
  const open = Array.from(doc.querySelectorAll<HTMLElement>('[aria-modal="true"]')).filter(
    (d) => isShown(d) && trapsFocus(d),
  );
  return open.length ? open[open.length - 1] : null;
}

/** The controls Tab stops on inside `root`, in document order. */
function tabbableIn(root: Element): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(TABBABLE)).filter(
    (el) => el.tabIndex >= 0 && isShown(el),
  );
}

/** Installs the wrap on `doc`; returns the uninstall function. */
export function installDialogTabWrap(doc: Document = document): () => void {
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key !== 'Tab' || e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey) return;
    const dialog = topmostModalDialog(doc);
    if (!dialog) return;
    const items = tabbableIn(dialog);
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    const active = doc.activeElement;
    // Focus on nothing (BODY): bring it back. Focus in a popup portalled
    // outside the dialog (a menu, a picker) is left alone.
    const lost = !active || active === doc.body;
    if (!e.shiftKey && (active === last || lost)) {
      e.preventDefault();
      first.focus();
    } else if (e.shiftKey && (active === first || lost)) {
      e.preventDefault();
      last.focus();
    }
  };
  doc.addEventListener('keydown', onKeyDown, true);
  return () => doc.removeEventListener('keydown', onKeyDown, true);
}
