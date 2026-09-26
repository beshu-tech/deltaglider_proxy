/**
 * Dirty-state registration for forms that keep their own field state (the
 * immediate-save IAM forms) instead of a single `useDirtySection` value.
 *
 * Thin wrapper over the public `useDirtySection` API: the tracked value is the
 * boolean itself against a `false` baseline, so a dirty form lights the
 * sidebar dot, the `● ` tab-title prefix and the beforeunload prompt exactly
 * like the section editors do.
 */
import { confirmDialog } from './confirmDialog';
import { useEffect, useState } from 'react';
import { getDirtySections, useDirtySection } from './useDirtySection';

export function useDirtyFlag(key: string, isDirty: boolean): void {
  const { setValue } = useDirtySection<boolean>(key, false);
  useEffect(() => {
    setValue(isDirty);
  }, [isDirty, setValue]);
}

/**
 * Structural "has the form moved off its baseline" check. `baseline` is
 * captured on mount and re-captured with `markClean()` after a successful save
 * (the forms are not remounted after a save, so the mount-time snapshot would
 * otherwise stay stale). Values must be JSON-serialisable.
 */
export function useFormBaseline<T>(current: T): { isDirty: boolean; markClean: () => void } {
  const serialised = JSON.stringify(current);
  const [baseline, setBaseline] = useState(serialised);
  return { isDirty: serialised !== baseline, markClean: () => setBaseline(serialised) };
}

/**
 * Ask before an in-panel action (row switch, "New", duplicate) throws away the
 * unsaved edits registered under `key`. True = go ahead.
 */
export async function confirmDiscardEdits(key: string): Promise<boolean> {
  if (!getDirtySections().has(key)) return true;
  return confirmDialog({
    title: 'Discard your unsaved changes?',
    content: 'You have unsaved changes on this form. They are lost if you continue.',
    okText: 'Discard changes',
    cancelText: 'Keep editing',
    danger: true,
  });
}

/**
 * Dirty/⌘S keys of the IAM forms. Each must appear in the matching leaf's
 * `dirtyKeys` in components/adminNavigation.tsx so the right sidebar dot lights.
 */
export const IAM_DIRTY_KEYS = {
  users: 'access/users',
  groups: 'access/groups',
  providers: 'access/external-auth/providers',
  mappingRules: 'access/external-auth/mapping-rules',
} as const;
