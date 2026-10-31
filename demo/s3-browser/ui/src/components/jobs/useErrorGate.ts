import { useCallback, useState } from 'react';

/**
 * When a field of a rules page shows its validation error — the rule of the
 * validation framework (`validation/useFieldValidation.ts`): once the field
 * lost focus, or after `submit()` (Review & apply), never while the user
 * types in it for the first time (a half-typed "3" is not an error yet).
 *
 * The rules page owns ONE gate for the page settings and for every field of
 * every rule (keys such as `tick` or `<rule uid>:batch_size`), so the dirty
 * bar can count the errors on screen across the drawer and the page.
 *
 * `props(key)` goes on the input (`onBlur`), `touch(key)` is the same for a
 * composite field, `show(key)` says whether the error is visible now.
 * `reset()` after an apply or a discard.
 */
export function useErrorGate() {
  const [left, setLeft] = useState<ReadonlySet<string>>(new Set());
  const [submitted, setSubmitted] = useState(false);
  const touch = useCallback(
    (key: string) => setLeft((cur) => (cur.has(key) ? cur : new Set(cur).add(key))),
    [],
  );
  const props = useCallback((key: string) => ({ onBlur: () => touch(key) }), [touch]);
  const show = useCallback((key: string) => submitted || left.has(key), [submitted, left]);
  const submit = useCallback(() => setSubmitted(true), []);
  const reset = useCallback(() => {
    setSubmitted(false);
    setLeft(new Set());
  }, []);
  return { props, touch, show, submit, reset };
}

export type ErrorGate = ReturnType<typeof useErrorGate>;
