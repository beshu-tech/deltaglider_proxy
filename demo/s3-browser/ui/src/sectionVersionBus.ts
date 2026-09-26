/**
 * Tab-local notice that a section PUT from THIS tab moved a section from
 * one version to the next. The server accepted the PUT's `If-Match`, so the
 * only change between the two versions is this tab's own edit. A sibling
 * editor of the same section that loaded the old version (the System page
 * stacks several `advanced` editors) adopts the new version, so its next
 * apply is not a false conflict. An editor that loaded any other version
 * keeps it, and its apply still gets the 409 it deserves.
 */
import type { SectionName } from './adminApi';

type Listener = (from: string, to: string) => void;

const listeners = new Map<SectionName, Set<Listener>>();

export function onSectionVersionAdvanced(section: SectionName, fn: Listener): () => void {
  let set = listeners.get(section);
  if (!set) {
    set = new Set();
    listeners.set(section, set);
  }
  set.add(fn);
  return () => {
    set.delete(fn);
  };
}

export function sectionVersionAdvanced(section: SectionName, from: string, to: string): void {
  for (const fn of listeners.get(section) ?? []) fn(from, to);
}
