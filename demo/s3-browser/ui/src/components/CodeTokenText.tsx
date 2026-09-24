import React from 'react';
import { splitCodeTokens } from '../codeTokens';

/** Renders plain text (often a server message) with CLI flags and `DGP_*`
 *  environment variable names in monospace `<code>`. */
export function CodeTokenText({ text }: { text: string }) {
  return (
    <>
      {splitCodeTokens(text).map((seg, i) =>
        seg.kind === 'code' ? (
          <code key={i} style={{ fontFamily: 'var(--font-mono)', fontSize: '0.92em' }}>
            {seg.text}
          </code>
        ) : (
          <React.Fragment key={i}>{seg.text}</React.Fragment>
        ),
      )}
    </>
  );
}
