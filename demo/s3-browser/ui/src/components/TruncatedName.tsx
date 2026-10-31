import { useLayoutEffect, useRef, useState, type CSSProperties } from 'react';
import { splitForMiddleEllipsis } from '../nameTruncation';

/**
 * TruncatedName — THE way to show an object key, a bucket name or a rule
 * name in a space that can be too narrow.
 *
 * A name that does not fit is cut in the MIDDLE: the end stays visible, so
 * `…-linux-amd64.tar.gz` and `…-linux-arm64.tar.gz`, or `releases-prod` and
 * `releases-dr`, never render as the same line. A plain
 * `text-overflow: ellipsis` cuts the end, which is where names differ.
 *
 *   <TruncatedName text={bucket.name} />                    // hover = full name
 *   <TruncatedName text={key} title={false} />              // the row has a title already
 *   <TruncatedName text={rule.name} style={{ fontWeight: 600 }} />
 *
 * The component is one flex line. Give its parent `minWidth: 0` (a flex or
 * grid child) or a `maxWidth`, so it can shrink. `tailChars` sets the most
 * characters kept at the end (20 by default).
 *
 * A name that fits stays ONE text node (so `getByText(name)` finds it); the
 * split into a cut head and a fixed tail happens only while the name
 * overflows, measured on layout and on every resize.
 */
export default function TruncatedName({
  text,
  style,
  title,
  tailChars,
  className,
}: {
  text: string;
  style?: CSSProperties;
  /** The hover text: the full text by default; `false` when an ancestor has one. */
  title?: string | false;
  tailChars?: number;
  className?: string;
}) {
  const { head, tail } = splitForMiddleEllipsis(text, tailChars);
  const rootRef = useRef<HTMLSpanElement>(null);
  const [split, setSplit] = useState(false);

  useLayoutEffect(() => {
    const root = rootRef.current;
    if (!root || !tail) {
      setSplit(false);
      return;
    }
    const check = () => {
      const headEl = root.firstElementChild as HTMLElement | null;
      if (!headEl) return;
      if (!split) {
        if (headEl.scrollWidth > headEl.clientWidth + 1) setSplit(true);
      } else {
        const tailEl = headEl.nextElementSibling as HTMLElement | null;
        const needed = headEl.scrollWidth + (tailEl?.scrollWidth ?? 0);
        if (needed <= root.clientWidth) setSplit(false);
      }
    };
    check();
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(check);
    observer.observe(root);
    return () => observer.disconnect();
  }, [split, text, tail]);

  const cut: CSSProperties = { overflow: 'hidden', textOverflow: 'ellipsis', minWidth: 0, flexShrink: 1 };
  return (
    <span
      ref={rootRef}
      className={className}
      title={title === false ? undefined : (title ?? text)}
      style={{ display: 'flex', minWidth: 0, maxWidth: '100%', whiteSpace: 'nowrap', ...style }}
    >
      {split && tail ? (
        <>
          <span style={cut}>{head}</span>
          <span style={{ flexShrink: 0, maxWidth: '75%', overflow: 'hidden' }}>{tail}</span>
        </>
      ) : (
        <span style={cut}>{text}</span>
      )}
    </span>
  );
}
