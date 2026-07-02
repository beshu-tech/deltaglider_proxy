import { Input } from 'antd';
import { useEffect, useRef, useState } from 'react';
import { lineList, lines } from './ruleEditorHelpers';

/**
 * Multi-line "one glob per line" editor backed by a `string[]`.
 *
 * ## Why this isn't just a controlled `<Input.TextArea>`
 *
 * The obvious `value={lines(arr)} onChange={e => onChange(lineList(e.target.value))}`
 * is BROKEN: `lineList` does `.filter(Boolean)`, so the instant you press Enter
 * (which makes a trailing blank line) the round-trip strips it and `value` snaps
 * back with no newline — you can never start a second line. Same failure mode as
 * the ConditionPrefixInput comma round-trip (see docs/plan admin-editor bug class).
 *
 * Fix: hold the RAW textarea string in local state while editing (newlines and
 * blank lines preserved), and only parse to the `string[]` on blur. The prop
 * re-seeds local state solely on a genuine external change (echo-guarded), so
 * programmatic updates still flow in without clobbering an in-progress edit.
 */
interface Props {
  value: string[];
  onChange: (value: string[]) => void;
  rows?: number;
  placeholder?: string;
  style?: React.CSSProperties;
}

export default function GlobListTextArea({ value, onChange, rows = 3, placeholder, style }: Props) {
  const [text, setText] = useState<string>(() => lines(value));
  // The last array we emitted, as a stable key, to tell our own echo apart from
  // a real external prop change.
  const lastEmitted = useRef<string>(JSON.stringify(lineList(lines(value))));

  useEffect(() => {
    const incoming = JSON.stringify(value);
    if (incoming === lastEmitted.current) return; // our own echo — ignore
    lastEmitted.current = incoming;
    setText(lines(value));
  }, [value]);

  const commit = () => {
    const parsed = lineList(text);
    const key = JSON.stringify(parsed);
    if (key !== lastEmitted.current) {
      lastEmitted.current = key;
      onChange(parsed);
    }
    // Re-render the textarea from the canonical parsed form so a trailing blank
    // line the user left is tidied once they're done — but only on blur.
    setText(lines(parsed));
  };

  return (
    <Input.TextArea
      value={text}
      onChange={(e) => setText(e.target.value)}
      onBlur={commit}
      rows={rows}
      placeholder={placeholder}
      style={style}
    />
  );
}
