import { useCallback, useState } from 'react';

/**
 * When a field may show its validation error: not while the user types in
 * it for the first time (a half-typed "3" is not an error yet), and always
 * after the user left it once. A value that is wrong from the start (the
 * user never focused the field) shows its error at once.
 *
 * `props(key)` goes on the input: `onFocus` / `onBlur`. `show(key)` says
 * whether the field's error is visible now.
 */
export function useErrorGate() {
  const [typing, setTyping] = useState<string | null>(null);
  const [left, setLeft] = useState<ReadonlySet<string>>(new Set());
  const props = useCallback(
    (key: string) => ({
      onFocus: () => setTyping(key),
      onBlur: () => {
        setTyping(null);
        setLeft((cur) => (cur.has(key) ? cur : new Set(cur).add(key)));
      },
    }),
    [],
  );
  const show = useCallback((key: string) => typing !== key || left.has(key), [typing, left]);
  return { props, show };
}
