// renderDoc.test.ts — VERSION_HEADING_RE parses changelog version headings.
// The same shape is matched on both render surfaces; this pins the contract
// against the real CHANGELOG corpus + future edge cases so a format tweak
// can't silently break the changelog page's hover-date / ToC logic.

import { describe, it, expect } from 'vitest';
import { VERSION_HEADING_RE, slugifyHeading, renderDoc } from './renderDoc';

const parse = (h: string) => {
  const m = h.match(VERSION_HEADING_RE);
  return m ? { version: m[1], date: m[2], title: m[3] } : null;
};

describe('VERSION_HEADING_RE', () => {
  it('parses every real changelog heading shape', () => {
    expect(parse('v0.1.9')).toEqual({ version: 'v0.1.9', date: undefined, title: undefined });
    expect(parse('v0.11.0 — 2026-05-16')).toEqual({ version: 'v0.11.0', date: '2026-05-16', title: undefined });
    expect(parse('v1.0.0 — 2026-05-22 — Project-shape milestone'))
      .toEqual({ version: 'v1.0.0', date: '2026-05-22', title: 'Project-shape milestone' });
  });

  it('handles future edge cases (prerelease, hyphen sep, multi-digit)', () => {
    expect(parse('v2.0.0-rc.1 — 2027-01-01')?.version).toBe('v2.0.0-rc.1');
    expect(parse('v1.5.0-beta — 2026-09-09 — Beta cut')).toEqual({ version: 'v1.5.0-beta', date: '2026-09-09', title: 'Beta cut' });
    expect(parse('v1.4.4 - 2026-07-01')?.date).toBe('2026-07-01'); // hyphen separator
    expect(parse('v10.20.30 — 2030-12-31')?.version).toBe('v10.20.30');
  });

  it('skips ordinary headings — even ones with a dash', () => {
    for (const neg of ['Added', 'Fixed', 'Fixed — CI', 'Removed (breaking)', 'version 1.2.3',
                       'Changed (breaking) — IAM permission templates are now `${iam:...}`']) {
      expect(parse(neg)).toBeNull();
    }
  });
});

describe('slugifyHeading', () => {
  it('matches the GitHub anchors the markdown links use', () => {
    // Each space becomes a dash; a removed "/" or "—" leaves a double dash.
    expect(slugifyHeading('Server / Advanced')).toBe('server--advanced');
    expect(slugifyHeading('Access — authentication')).toBe('access--authentication');
    expect(slugifyHeading('Jobs — one surface for everything background')).toBe('jobs--one-surface-for-everything-background');
    expect(slugifyHeading('502 Bad Gateway / 504 Gateway Timeout on large uploads'))
      .toBe('502-bad-gateway--504-gateway-timeout-on-large-uploads');
    expect(slugifyHeading('The `legacy_key` shim')).toBe('the-legacy_key-shim');
  });
});

describe('renderDoc tables', () => {
  it('labels each body cell with its header and stacks tables of 3+ columns', async () => {
    const html = await renderDoc(
      '| Operation | Status | Notes |\n|---|---|---|\n| `GetObject` | Full | Delta-decoded |\n',
      'reference/x.md',
    );
    expect(html).toContain('<table class="docs-table-stack">');
    expect(html).toContain('<td data-label="Operation">');
    expect(html).toContain('<td data-label="Status">Full</td>');
    expect(html).toContain('<td data-label="Notes">Delta-decoded</td>');
  });

  it('labels a two-column table but does not stack it', async () => {
    const html = await renderDoc('| Key | Value |\n|---|---|\n| a | b |\n', 'reference/x.md');
    expect(html).not.toContain('docs-table-stack');
    expect(html).toContain('<td data-label="Value">b</td>');
  });
});

describe('renderDoc heading anchors', () => {
  it('keeps the "#" anchor out of the search index', async () => {
    const html = await renderDoc('## Scope\n\nText.\n', 'reference/x.md');
    expect(html).toMatch(/<a class="docs-heading-anchor"[^>]*data-pagefind-ignore/);
  });
});

describe('themed screenshots', () => {
  it('turns one theme-neutral shot into a light and a dark image', async () => {
    const html = await renderDoc(
      '![The backends page lists three backends.](/_/screenshots/route-bucket-add-backend.webp)\n',
      'how-to/route-a-bucket-to-a-backend.md',
    );
    expect(html).toContain('src="/screenshots/route-bucket-add-backend.light.webp"');
    expect(html).toContain('src="/screenshots/route-bucket-add-backend.dark.webp"');
    expect(html.match(/class="shot-light"/g)).toHaveLength(1);
    expect(html.match(/class="shot-dark"/g)).toHaveLength(1);
    expect(html.match(/alt="The backends page lists three backends."/g)).toHaveLength(2);
    expect(html.match(/loading="lazy"/g)).toHaveLength(2);
  });

  it('leaves any other image as one image', async () => {
    const html = await renderDoc('![Old shot.](/_/screenshots/storage_backends.jpg)\n', 'x.md');
    expect(html.match(/<img/g)).toHaveLength(1);
    expect(html).toContain('src="/screenshots/storage_backends.jpg"');
    expect(html).not.toContain('shot-light');
  });
});
