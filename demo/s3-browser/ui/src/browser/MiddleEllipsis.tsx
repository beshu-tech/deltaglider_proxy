import type { CSSProperties } from 'react';
import { splitForMiddleEllipsis } from './nameTruncation';

/**
 * One line of text that, when too wide, is cut in the MIDDLE: the end of
 * the name stays visible, so `…-linux-amd64.tar.gz` and `…-linux-arm64.tar.gz`
 * do not render as the same line.
 */
export default function MiddleEllipsis({ text, style, title }: {
  text: string;
  style?: CSSProperties;
  /** The hover text: the full text by default; `false` when an ancestor has one. */
  title?: string | false;
}) {
  const { head, tail } = splitForMiddleEllipsis(text);
  return (
    <span title={title === false ? undefined : (title ?? text)} style={{ display: 'flex', minWidth: 0, whiteSpace: 'nowrap', ...style }}>
      <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', minWidth: 0, flexShrink: 1 }}>{head}</span>
      {tail && <span style={{ flexShrink: 0, maxWidth: '75%', overflow: 'hidden' }}>{tail}</span>}
    </span>
  );
}
