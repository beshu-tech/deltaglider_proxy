/**
 * Pure history-entry helpers behind `useBackClosesModal` (useOverlayClose.ts).
 * They take a History-like object so src/__tests__/modalHistory.test.ts
 * can drive them with a fake stack.
 */
type HistoryLike = Pick<History, 'state' | 'pushState' | 'back'>;

/** Push a same-URL entry tagged `dgModal: id`, keeping the current state's keys. */
export function pushModalEntry(history: HistoryLike, id: string, href: string): void {
  const prev: unknown = history.state;
  const base = prev && typeof prev === 'object' ? prev : {};
  history.pushState({ ...base, dgModal: id }, '', href);
}

/**
 * Pop the modal's entry after a non-Back close. Only when it is still the
 * current entry: if a navigation was pushed on top, Back would undo that
 * navigation instead, so the entry is left alone.
 */
export function popModalEntryIfTop(history: HistoryLike, id: string): boolean {
  const top: unknown = history.state;
  if (top && typeof top === 'object' && (top as { dgModal?: unknown }).dgModal === id) {
    history.back();
    return true;
  }
  return false;
}
