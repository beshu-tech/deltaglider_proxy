// docDrawings.test.ts — every drawing slot names a real page and a heading
// that the real renderer emits. A renamed heading fails here; without this
// test the drawing leaves the page and nobody sees it go.
import { describe, it, expect } from 'vitest';
import { DOC_DRAWINGS, splitForDrawings } from './docDrawings';
import { docContent } from './docContent';
import { renderDoc } from './renderDoc';

describe('docs drawings', () => {
  it('each slot cuts its page at an existing heading', async () => {
    for (const [path, slots] of Object.entries(DOC_DRAWINGS)) {
      const markdown = docContent(path);
      expect(markdown, `no doc at ${path}`).toBeTruthy();
      const html = await renderDoc(markdown!, path);
      const parts = splitForDrawings(html, slots);
      expect(parts.filter((p) => 'drawing' in p).length, `${path}: a heading id is gone`).toBe(slots.length);
      expect(parts.map((p) => ('html' in p ? p.html : '')).join('')).toBe(html);
    }
  });

  it('puts the drawing before its heading', () => {
    const html = '<h1 id="t">T</h1><p>a</p><h2 id="x">X</h2><p>b</p>';
    expect(splitForDrawings(html, [{ before: 'x', drawing: 'read-path' }])).toEqual([
      { html: '<h1 id="t">T</h1><p>a</p>' },
      { drawing: 'read-path' },
      { html: '<h2 id="x">X</h2><p>b</p>' },
    ]);
  });

  it('keeps the page whole when a heading is absent', () => {
    expect(splitForDrawings('<p>a</p>', [{ before: 'gone', drawing: 'read-path' }])).toEqual([{ html: '<p>a</p>' }]);
  });
});
