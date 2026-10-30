// docText.test.ts — extractSummary truncation.
import { describe, expect, it } from 'vitest';
import { extractSummary } from './docText';

describe('extractSummary', () => {
  it('keeps a short first paragraph whole', () => {
    expect(extractSummary('# T\n\nShort text.\n', 90)).toBe('Short text.');
  });

  it('cuts a long paragraph at a word boundary, never mid-word', () => {
    const md = '# T\n\nRun the proxy, upload two firmware versions, and see the second one stored as a small delta.\n';
    const s = extractSummary(md, 60);
    expect(s.endsWith('…')).toBe(true);
    expect(s.length).toBeLessThanOrEqual(60);
    const words = md.split('\n')[2].split(' ');
    for (const w of s.slice(0, -1).split(' ')) expect(words).toContain(w.replace(/,$/, ',') );
  });

  it('drops trailing punctuation before the ellipsis', () => {
    const s = extractSummary('# T\n\nAlpha beta gamma, delta epsilon zeta eta theta.\n', 20);
    expect(s).toBe('Alpha beta gamma…');
  });

  it('keeps the characters inside a code span', () => {
    expect(extractSummary('# T\n\n*Every endpoint under `/_/api/admin/*`, grouped by purpose.*\n'))
      .toBe('Every endpoint under /_/api/admin/*, grouped by purpose.');
    expect(extractSummary('# T\n\nSet `config_sync_bucket` to __share__ the DB.\n'))
      .toBe('Set config_sync_bucket to share the DB.');
  });
});
