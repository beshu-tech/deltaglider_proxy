import React from 'react';
import { splitLinkSegments } from '../linkifyDocUrl';

/** Renders a plain server message with https:// URLs as clickable links.
 *  deltaglider.com/docs URLs rewrite to the in-app docs viewer; everything
 *  opens in a new tab so modal/dirty state is never lost. */
export function LinkifiedText({ text }: { text: string }) {
  return (
    <>
      {splitLinkSegments(text).map((seg, i) =>
        seg.kind === 'link' ? (
          <a key={i} href={seg.href} target="_blank" rel="noopener noreferrer">
            {seg.text}
          </a>
        ) : (
          <React.Fragment key={i}>{seg.text}</React.Fragment>
        ),
      )}
    </>
  );
}
