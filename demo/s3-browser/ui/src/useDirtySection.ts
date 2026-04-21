/**
 * useDirtySection — dirty-state coordination hook.
 *
 * Each config section panel calls this hook with its current form
 * state + last-applied state. Returns:
 *
 *   - `isDirty` — drives the sidebar amber dot and right-rail Apply /
 *     Discard buttons.
 *   - `discard()` — resets the form to the applied snapshot.
 *
 * Also:
 *   - Sets the browser tab title prefix (`● `) when any section is
 *     dirty, so the operator sees a quick visual cue outside the UI
 *     itself (§5.2 of the admin UI revamp plan).
 *   - Registers a `beforeunload` handler that prompts on page close
 *     when any section is dirty.
 *
 * ## Cross-section dirty state
 *
 * A module-level `dirtySections` Set tracks which sections are
 * currently dirty. Each section registers itself by `name` so the
 * sidebar can inspect the set; the Set is a module singleton (React
 * state is local to a component, which doesn't help cross-panel
 * signalling). The hook's cleanup removes the section on unmount.
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import type { SectionName } from './adminApi';

// Module singleton. Section panels mount concurrently in the new nav;
// we need the dirty set to live outside any one component.
const dirtySections = new Set<SectionName>();
const dirtyListeners = new Set<() => void>();

function notifyDirtyListeners() {
  for (const l of dirtyListeners) l();
}

/** Subscribe to global dirty-state changes. The Sidebar uses this to
 *  render the amber dot on each section entry. */
export function subscribeToDirtyState(listener: () => void): () => void {
  dirtyListeners.add(listener);
  return () => {
    dirtyListeners.delete(listener);
  };
}

/** Snapshot the current dirty set — used by components outside
 *  a section panel to render indicators. */
export function getDirtySections(): Set<SectionName> {
  return new Set(dirtySections);
}

export interface UseDirtySectionResult<T> {
  /** Current (editable) form state. */
  value: T;
  /** True when `value` differs from the last applied snapshot. */
  isDirty: boolean;
  /** Update the form state. */
  setValue: (next: T) => void;
  /** Revert to the snapshot. */
  discard: () => void;
  /** Reset the snapshot to `value` — call after a successful Apply. */
  markApplied: () => void;
  /** Replace the snapshot outright (e.g. when the server resends new state). */
  resetWith: (next: T) => void;
}

/**
 * Deep-equal that's fine for form state: JSON serialisation is the
 * reference. Fails closed — if serialisation throws, we treat as
 * dirty so the operator can't accidentally skip an Apply.
 */
function shallowEq<T>(a: T, b: T): boolean {
  try {
    return JSON.stringify(a) === JSON.stringify(b);
  } catch {
    return false;
  }
}

export function useDirtySection<T>(
  section: SectionName,
  initial: T
): UseDirtySectionResult<T> {
  const [value, setValueState] = useState<T>(initial);
  const snapshotRef = useRef<T>(initial);
  // `value` referenced inside `markApplied` must come from a ref so
  // the callback identity doesn't change on every render (which
  // would cascade through `useCallback` consumers and cause infinite
  // refresh loops when the callback ends up in a `useEffect`'s deps).
  const valueRef = useRef<T>(value);
  valueRef.current = value;

  const isDirty = !shallowEq(value, snapshotRef.current);

  // Keep the module-level set in sync so the Sidebar / tab title can
  // react to our dirty state. The effect fires on every isDirty flip.
  useEffect(() => {
    if (isDirty) {
      dirtySections.add(section);
    } else {
      dirtySections.delete(section);
    }
    notifyDirtyListeners();
    return () => {
      // On unmount, clean up so a section panel that dismounts with
      // unsaved state doesn't leave the sidebar flashing forever.
      dirtySections.delete(section);
      notifyDirtyListeners();
    };
  }, [section, isDirty]);

  // All callbacks are `useCallback`'d with stable deps so panels
  // that put them in `useCallback` / `useEffect` dependency arrays
  // don't re-run on every parent render.
  const setValue = useCallback((next: T) => setValueState(next), []);
  const discard = useCallback(() => setValueState(snapshotRef.current), []);
  const markApplied = useCallback(() => {
    snapshotRef.current = valueRef.current;
    setValueState(valueRef.current); // trigger isDirty recompute
  }, []);
  const resetWith = useCallback((next: T) => {
    snapshotRef.current = next;
    setValueState(next);
  }, []);

  return { value, isDirty, setValue, discard, markApplied, resetWith };
}

/**
 * Global side-effect hook: tab title prefix + beforeunload prompt.
 * Mount once at the app root (AdminPage or higher). Idempotent —
 * calling in multiple places is OK; the module-level set is shared.
 */
export function useDirtyGlobalIndicators() {
  useEffect(() => {
    const originalTitle = document.title;

    const updateTitle = () => {
      if (dirtySections.size > 0) {
        if (!document.title.startsWith('● ')) {
          document.title = '● ' + document.title.replace(/^● /, '');
        }
      } else {
        document.title = document.title.replace(/^● /, '');
      }
    };
    const unsub = subscribeToDirtyState(updateTitle);
    updateTitle(); // initial

    const beforeUnload = (e: BeforeUnloadEvent) => {
      if (dirtySections.size > 0) {
        e.preventDefault();
        // `returnValue` is required for Chrome legacy.
        e.returnValue = 'You have unsaved config changes. Leave anyway?';
        return e.returnValue;
      }
    };
    window.addEventListener('beforeunload', beforeUnload);

    return () => {
      unsub();
      window.removeEventListener('beforeunload', beforeUnload);
      document.title = originalTitle;
    };
  }, []);
}
