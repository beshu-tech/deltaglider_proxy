import { useEffect, useRef } from 'react';
import type { RefObject } from 'react';

type AnyRef<T extends HTMLElement = HTMLElement> = RefObject<T | null>;

function useLatest<T>(value: T) {
  const ref = useRef(value);
  useEffect(() => {
    ref.current = value;
  }, [value]);
  return ref;
}

export function useDocumentEvent<K extends keyof DocumentEventMap>(
  type: K,
  handler: (event: DocumentEventMap[K]) => void,
  enabled = true
) {
  const handlerRef = useLatest(handler);

  useEffect(() => {
    if (!enabled) return;

    const listener = (event: DocumentEventMap[K]) => handlerRef.current(event);
    document.addEventListener(type, listener as EventListener);
    return () => document.removeEventListener(type, listener as EventListener);
  }, [enabled, handlerRef, type]);
}

export function useOnClickOutside(
  refs: AnyRef[],
  onOutside: () => void,
  enabled = true
) {
  useDocumentEvent(
    'mousedown',
    (event) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (refs.some((ref) => ref.current?.contains(target))) return;
      onOutside();
    },
    enabled
  );
}

export function useEscapeKey(onEscape: () => void, enabled = true) {
  useDocumentEvent(
    'keydown',
    (event) => {
      if (event.key === 'Escape') onEscape();
    },
    enabled
  );
}
