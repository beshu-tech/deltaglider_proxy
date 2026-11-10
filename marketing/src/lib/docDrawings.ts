// docDrawings.ts — which docs page gets which drawing, and where.
// The markdown in docs/product stays plain: the product viewer, GitHub and
// the .md endpoints read the same files and cannot render a drawing. The
// website puts each drawing at the end of a section, before the heading
// named here. docDrawings.test.ts fails when a heading id no longer exists.

export type DocDrawing = 'write-path' | 'read-path' | 'key-custody';

export interface DocDrawingSlot {
  /** Id of the heading that the drawing comes before. */
  before: string;
  drawing: DocDrawing;
}

/** Indexed by the docs/product path of the page (no extension). */
export const DOC_DRAWINGS: Record<string, DocDrawingSlot[]> = {
  'explanation/delta-compression': [
    { before: 'the-get-path-and-why-you-can-trust-it', drawing: 'write-path' },
  ],
  'tutorials/first-delta-savings': [
    { before: 'what-you-built', drawing: 'read-path' },
  ],
  'explanation/encryption-at-rest': [
    { before: 'why-a-bucket-can-have-its-own-key', drawing: 'key-custody' },
  ],
};

export type DocPart = { html: string } | { drawing: DocDrawing };

/**
 * Cut rendered doc HTML at the headings of `slots`: the HTML before each
 * heading, then its drawing. A slot whose heading is absent is skipped
 * (the test refuses that state), so the page still renders whole.
 */
export function splitForDrawings(html: string, slots: DocDrawingSlot[]): DocPart[] {
  const cuts = slots
    .map((s) => ({ at: html.search(new RegExp(`<h[2-6][^>]*\\bid="${s.before}"`)), drawing: s.drawing }))
    .filter((c) => c.at >= 0)
    .sort((a, b) => a.at - b.at);
  const parts: DocPart[] = [];
  let from = 0;
  for (const c of cuts) {
    parts.push({ html: html.slice(from, c.at) }, { drawing: c.drawing });
    from = c.at;
  }
  parts.push({ html: html.slice(from) });
  return parts;
}
