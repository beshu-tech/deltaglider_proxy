import { useCallback, useRef } from 'react';
import type { RefObject } from 'react';
import { useOnClickOutside } from './useDocumentEvent';
import { useFixedOverlayPosition } from './useFixedOverlayPosition';

/**
 * Fixed-overlay lifecycle for `SimpleAutoComplete` — the app's hand-rolled
 * free-text-with-suggestions input and this hook's only consumer. (Plain
 * selects use AntD <Select> directly; the old SimpleSelect fork was removed
 * once we confirmed AntD 6's Select popup works fine here — it never injected
 * the body scroll-lock that motivated the fork.) Owns the trigger ref, the
 * overlay element handle, below-trigger anchored positioning, and
 * click-outside close.
 */

interface Options {
  /** Whether the overlay is currently shown. Drives positioning + listeners. */
  visible: boolean;
  /** Called when a click outside should dismiss the overlay. */
  onClose: () => void;
}

interface OverlayDropdown<T extends HTMLElement> {
  /** Ref for the anchor/trigger element. */
  triggerRef: RefObject<T>;
  /** Callback ref for the overlay element — feeds click-outside detection. */
  setOverlay: (el: HTMLDivElement | null) => void;
  /** Anchored coordinates from `useFixedOverlayPosition`. */
  pos: ReturnType<typeof useFixedOverlayPosition>;
}

export function useOverlayDropdown<T extends HTMLElement = HTMLDivElement>({
  visible,
  onClose,
}: Options): OverlayDropdown<T> {
  // Initialised null like the call site's original `useRef<T>(null)`, so the
  // returned ref types as `RefObject<T>` and slots straight onto a JSX `ref`.
  const triggerRef = useRef<T>(null) as RefObject<T>;
  const overlayRef = useRef<HTMLDivElement | null>(null);
  const setOverlay = useCallback((el: HTMLDivElement | null) => {
    overlayRef.current = el;
  }, []);

  const pos = useFixedOverlayPosition(triggerRef, visible);

  useOnClickOutside([triggerRef, overlayRef], onClose, visible);

  return { triggerRef, setOverlay, pos };
}
