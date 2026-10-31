/**
 * useFieldValidation — runs the validators of a form's fields on blur and
 * on submit (UX harvest D9, D10, D46).
 *
 *   const form = useFieldValidation();
 *   const cache = form.number('cache', value.cache_size_mb,
 *     (n) => setValue({ ...value, cache_size_mb: n }), v.optional(v.integer({ min: 16, max: 65536 })));
 *   const listen = form.text('listen', value.listen_addr ?? '',
 *     (s) => setValue({ ...value, listen_addr: s || undefined }), v.optional(v.hostPort()));
 *
 *   <FormField label="Reference cache size (MiB)" error={cache.error}>
 *     <Input {...cache.inputProps} suffix="MiB" />
 *   </FormField>
 *   <StickyDirtyBar errorCount={form.issueCount} onApply={() => form.submit() && apply()} … />
 *
 * Rules:
 *  - An error shows once the field lost focus (or after `submit()`), then
 *    follows each keystroke. FormField turns `error` into the sentence under
 *    the control plus `aria-invalid` and the red border.
 *  - `issueCount` counts the errors on screen: the dirty bar's "N issues
 *    to fix". `submit()` shows every error and returns true when there is none.
 *  - A number field is a TEXT input (inputMode="numeric"). The typed text
 *    stays as typed: an invalid text ("-1", "abc") shows its error and the
 *    stored value keeps the last valid number. Nothing is clamped.
 *  - An outside change of the value (Discard, an example chip) replaces the
 *    text.
 *  - `reset()` forgets "touched" (after an apply, or a discard).
 *
 * Field names must be stable; call `text`/`number` in the same order each
 * render (they are not hooks, but the counts follow the order).
 */
import { useCallback, useRef, useState } from 'react';
import type { Validator } from './validators';

interface FieldBinding {
  /** The sentence to show (only once touched or submitted), else undefined. */
  error: string | undefined;
  /** Spread onto an AntD <Input> (or a native input). */
  inputProps: {
    value: string;
    onChange: (e: { target: { value: string } }) => void;
    onBlur: () => void;
    inputMode?: 'numeric';
  };
}

export interface FieldValidation {
  text(name: string, value: string, onChange: (value: string) => void, validator: Validator): FieldBinding;
  number(
    name: string,
    value: number | null | undefined,
    onChange: (value: number | undefined) => void,
    validator: Validator,
  ): FieldBinding;
  /** Errors on screen: the dirty bar's "N issues to fix". Read it AFTER
   *  binding the fields (below them in the JSX). */
  readonly issueCount: number;
  /** Shows every error; true when the form has none. */
  submit(): boolean;
  /** Forgets touched fields and typed drafts. */
  reset(): void;
}

interface Draft {
  text: string;
  /** The stored value when the text was typed: another value = an outside change. */
  base: number | undefined;
}

export function useFieldValidation(): FieldValidation {
  const [touched, setTouched] = useState<ReadonlySet<string>>(() => new Set());
  const [submitted, setSubmitted] = useState(false);
  const [drafts, setDrafts] = useState<ReadonlyMap<string, Draft>>(() => new Map());
  // Every field's current error, shown or not, for submit() (an event handler).
  const errorsRef = useRef(new Map<string, string>());
  const rendered = new Map<string, string>();
  let issueCount = 0;

  const touch = (name: string) =>
    setTouched((prev) => (prev.has(name) ? prev : new Set(prev).add(name)));

  const bind = (name: string, text: string, setText: (t: string) => void, validator: Validator, numeric: boolean): FieldBinding => {
    const error = validator(text);
    if (error) rendered.set(name, error);
    const shown = error && (submitted || touched.has(name)) ? error : undefined;
    if (shown) issueCount += 1;
    errorsRef.current = rendered;
    return {
      error: shown,
      inputProps: {
        value: text,
        onChange: (e) => setText(e.target.value),
        onBlur: () => touch(name),
        ...(numeric ? { inputMode: 'numeric' as const } : {}),
      },
    };
  };

  const text: FieldValidation['text'] = (name, value, onChange, validator) =>
    bind(name, value, onChange, validator, false);

  const number: FieldValidation['number'] = (name, value, onChange, validator) => {
    const stored = value ?? undefined;
    const draft = drafts.get(name);
    const shownText = draft && draft.base === stored ? draft.text : stored === undefined ? '' : String(stored);
    return bind(
      name,
      shownText,
      (typed) => {
        const valid = validator(typed) === null;
        const next = valid ? (typed.trim() === '' ? undefined : Number(typed.trim())) : stored;
        setDrafts((prev) => new Map(prev).set(name, { text: typed, base: next }));
        if (valid && next !== stored) onChange(next);
      },
      validator,
      true,
    );
  };

  const submit = useCallback(() => {
    setSubmitted(true);
    return errorsRef.current.size === 0;
  }, []);

  const reset = useCallback(() => {
    setSubmitted(false);
    setTouched(new Set());
    setDrafts(new Map());
  }, []);

  return {
    text,
    number,
    // A getter: the fields bind after this hook returns, so the count is
    // read when the bar renders, below the fields.
    get issueCount() {
      return issueCount;
    },
    submit,
    reset,
  };
}
