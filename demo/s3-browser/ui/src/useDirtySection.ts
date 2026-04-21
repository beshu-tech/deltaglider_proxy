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
 * A module-level **refcount Map** (`SectionName` -> number of
 * currently-mounted dirty panels for that section) tracks which
 * sections are dirty. Wave 5 introduces multiple panels under the
 * same SectionName (`access/credentials`, `access/users`,
 * `access/groups`, `access/ext-auth`) so the earlier Set-based
 * design would clobber siblings: one panel unmount would `delete`
 * the `access` bit even though another sibling still carries
 * dirty state. Refcounting is the minimum correct primitive.
 *
 * Consumers that want the set of dirty sections call
 * [`getDirtySections`] — returns a snapshot Set derived from the
 * refcount map.
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import type { SectionName } from './adminApi';

// Module singletons. Section panels mount concurrently in the new
// nav; we need the dirty state to live outside any one component.
const dirtyCounts = new Map<SectionName, number>();
const dirtyListeners = new Set<() => void>();

function notifyDirtyListeners() {
  for (const l of dirtyListeners) l();
}

/** Increment the dirty refcount for a section. */
function addDirty(section: SectionName): void {
  dirtyCounts.set(section, (dirtyCounts.get(section) ?? 0) + 1);
}

/** Decrement the dirty refcount; remove the key when it reaches 0. */
function removeDirty(section: SectionName): void {
  const n = dirtyCounts.get(section) ?? 0;
  if (n <= 1) {
    dirtyCounts.delete(section);
  } else {
    dirtyCounts.set(section, n - 1);
  }
}

/** Subscribe to global dirty-state changes. The Sidebar uses this to
 *  render the amber dot on each section entry. */
export function subscribeToDirtyState(listener: () => void): () => void {
  dirtyListeners.add(listener);
  return () => {
    dirtyListeners.delete(listener);
  };
}

/** Snapshot the current dirty sections — derived from the refcount
 *  Map, filtering out entries at zero. Used by components outside
 *  a section panel to render indicators (sidebar dot, tab title). */
export function getDirtySections(): Set<SectionName> {
  return new Set(dirtyCounts.keys());
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
 * Structural equality via JSON serialisation with a stable key
 * order. Good enough for the form-state shapes admin panels use
 * (plain objects / arrays / scalars).
 *
 * Key-order stability matters: the server's `serde_json` may
 * re-serialise object keys in a different order than the client's
 * literal, which would make a naïve `JSON.stringify(a) ===
 * JSON.stringify(b)` report `isDirty = true` forever after Apply
 * — the operator sees the Apply button stay active, re-applies,
 * and gets a confusing no-op loop. Sorting keys recursively
 * canonicalises both sides so equality is purely structural.
 *
 * Chokes on circular refs (we treat the throw as "different" so
 * the operator can't accidentally skip an Apply).
 */
function jsonEq<T>(a: T, b: T): boolean {
  try {
    return stableStringify(a) === stableStringify(b);
  } catch {
    return false;
  }
}

/**
 * JSON.stringify that sorts object keys recursively so equal
 * values always serialise to the same byte sequence regardless of
 * the insertion order. Does not traverse into arrays (arrays are
 * order-significant by contract) — their elements are recursively
 * normalised but not reordered.
 */
function stableStringify(value: unknown): string {
  return JSON.stringify(value, (_key, v) => {
    if (v === null || typeof v !== 'object' || Array.isArray(v)) return v;
    const sorted: Record<string, unknown> = {};
    for (const k of Object.keys(v as Record<string, unknown>).sort()) {
      sorted[k] = (v as Record<string, unknown>)[k];
    }
    return sorted;
  });
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

  const isDirty = !jsonEq(value, snapshotRef.current);

  // Keep the module-level set in sync so the Sidebar / tab title can
  // react to our dirty state. The effect fires on every isDirty flip.
  //
  // Refcounted: each dirty panel contributes +1 to the section's
  // count; unmount / isDirty-flips-to-false release the refcount.
  // The sidebar sees `dirty` when the count is > 0, so sibling
  // panels sharing a SectionName (Wave 5's 4 Access panels) each
  // maintain their own bit without clobbering the others'.
  useEffect(() => {
    if (!isDirty) {
      // Not dirty on this render; nothing to register for this
      // effect run. No cleanup either — the PREVIOUS effect run's
      // cleanup (if it was dirty) already removed the refcount.
      return;
    }
    addDirty(section);
    notifyDirtyListeners();
    return () => {
      removeDirty(section);
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
      if (dirtyCounts.size > 0) {
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
      if (dirtyCounts.size > 0) {
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
