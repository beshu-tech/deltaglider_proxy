// highlight.ts — syntax highlighting for doc code fences via shiki (already a
// transitive Astro dep). A single highlighter is created once and reused across
// every doc at build time. The theme is a custom dark palette tuned to the
// DeltaGlider brand (cyan-forward on the near-black --bg), so highlighted code
// feels native to the "technical logbook" surface rather than a stock theme.

import { createHighlighter, type Highlighter } from 'shiki';

// Languages the product docs actually use (see `grep '```' docs/product`).
// `promql` isn't a built-in grammar — fall back to a close cousin.
const LANGS = ['yaml', 'bash', 'shellscript', 'json', 'text', 'python', 'nginx', 'toml', 'dockerfile', 'ini'] as const;

// Map fence languages that shiki doesn't ship a grammar for onto a close one.
const LANG_ALIAS: Record<string, string> = {
  sh: 'bash',
  shell: 'bash',
  promql: 'text',   // PromQL has no shiki grammar; render as plain (still themed).
  yml: 'yaml',
  conf: 'nginx',
};

// Brand-tuned theme. Background matches the docs `pre` surface (#1d2336 —
// LIGHTER than the page --bg #141a2b so the code panel reads as a distinct,
// copyable surface). Shiki writes this as an inline background-color on the
// emitted <pre>, which would otherwise win over the CSS panel colour, so it
// must be kept in lockstep with --doc-code-bg in docs.css. The signal cyan is
// reserved for the tokens the eye should catch first (keys, functions), with
// calmer hues for strings/numbers — syntax FOREGROUNDS are unchanged.
const DG_THEME = {
  name: 'deltaglider-dark',
  type: 'dark' as const,
  colors: {
    'editor.background': '#1d2336',
    'editor.foreground': '#cbd5e1',
  },
  settings: [
    { settings: { foreground: '#cbd5e1', background: '#1d2336' } },
    { scope: ['comment', 'punctuation.definition.comment'], settings: { foreground: '#5b6b86', fontStyle: 'italic' } },
    { scope: ['string', 'string.quoted', 'meta.string'], settings: { foreground: '#8fd6c4' } },
    { scope: ['constant.numeric', 'constant.language', 'constant.character'], settings: { foreground: '#f0a868' } },
    { scope: ['keyword', 'storage.type', 'storage.modifier', 'keyword.control'], settings: { foreground: '#67e8f9' } },
    { scope: ['entity.name.function', 'support.function', 'meta.function-call'], settings: { foreground: '#22d3ee' } },
    // YAML/JSON keys + tags — the structural skeleton, in bright cyan.
    { scope: ['entity.name.tag', 'support.type.property-name', 'meta.object-literal.key', 'entity.name.tag.yaml'], settings: { foreground: '#67e8f9' } },
    { scope: ['variable', 'variable.other', 'meta.mapping.key'], settings: { foreground: '#e2e8f0' } },
    { scope: ['punctuation', 'meta.brace', 'punctuation.separator'], settings: { foreground: '#64748b' } },
    // Shell: command names pop, flags calmer.
    { scope: ['entity.name.command', 'support.function.builtin.shell'], settings: { foreground: '#22d3ee' } },
    { scope: ['variable.parameter', 'variable.language'], settings: { foreground: '#d6b3f0' } },
    { scope: ['markup.bold'], settings: { fontStyle: 'bold' } },
  ],
};

let highlighterPromise: Promise<Highlighter> | null = null;

async function getHighlighter(): Promise<Highlighter> {
  if (!highlighterPromise) {
    highlighterPromise = createHighlighter({
      themes: [DG_THEME as any],
      langs: LANGS as unknown as string[],
    });
  }
  return highlighterPromise;
}

/** Resolve a fence language to a grammar shiki has loaded; '' for unknown. */
function resolveLang(lang: string, hl: Highlighter): string {
  const l = (LANG_ALIAS[lang] ?? lang).toLowerCase();
  return hl.getLoadedLanguages().includes(l as any) ? l : '';
}

/**
 * Highlight one code block. Returns shiki's `<pre class="shiki">…</pre>` HTML
 * string (tokens wrapped in coloured spans). On an unknown language it still
 * themes the block (plain foreground) so every fence shares one surface.
 */
export async function highlightCode(code: string, lang: string): Promise<string> {
  const hl = await getHighlighter();
  const resolved = resolveLang(lang, hl);
  return hl.codeToHtml(code, {
    lang: resolved || 'text',
    theme: 'deltaglider-dark',
  });
}

/** Warm the highlighter once before a render pass (so the first doc isn't slow). */
export async function warmHighlighter(): Promise<void> {
  await getHighlighter();
}
