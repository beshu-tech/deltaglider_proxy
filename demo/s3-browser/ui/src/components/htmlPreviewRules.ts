/**
 * The sandboxed HTML preview: what the iframe may do, and the document it
 * renders. Pure, so the security rules are unit-tested.
 *
 * The page runs in an opaque origin (`sandbox` WITHOUT `allow-same-origin`):
 * it cannot read this window, the in-memory S3 credentials or the session
 * cookie, whatever its scripts do. An injected CSP decides what it may load:
 * nothing from the network unless the user allows remote content, and no
 * script unless the user allows scripts. Even with scripts on, it cannot
 * send requests (`connect-src 'none'`).
 */

export interface HtmlPreviewOptions {
  /** Run the page's scripts (inline and, with `remote`, external). */
  scripts: boolean;
  /** Load images, styles, fonts and media over https. */
  remote: boolean;
}

/** Flags of the iframe `sandbox` attribute. Never `allow-same-origin`. */
export function sandboxFlags({ scripts }: HtmlPreviewOptions): string {
  return scripts ? 'allow-scripts' : '';
}

/** The Content-Security-Policy that the preview injects into the page. */
export function previewCsp({ scripts, remote }: HtmlPreviewOptions): string {
  const local = "data: blob:";
  const fetchable = remote ? `${local} https:` : local;
  const script = scripts ? `'unsafe-inline'${remote ? ' https:' : ''}` : "'none'";
  return [
    "default-src 'none'",
    `img-src ${fetchable}`,
    `media-src ${fetchable}`,
    `font-src ${fetchable}`,
    `style-src 'unsafe-inline' ${fetchable}`,
    `script-src ${script}`,
    "connect-src 'none'",
    "form-action 'none'",
    "frame-src 'none'",
    "object-src 'none'",
    "base-uri 'none'",
  ].join('; ');
}

const escapeAttr = (s: string) => s.replace(/&/g, '&amp;').replace(/"/g, '&quot;');

/**
 * `html` with the CSP `<meta>` as the first element of the document. A meta
 * CSP applies only to content after it, so it goes right after the doctype
 * (keeping standards mode) and before any of the page's own markup.
 */
export function buildPreviewDocument(html: string, opts: HtmlPreviewOptions): string {
  const meta =
    `<meta http-equiv="Content-Security-Policy" content="${escapeAttr(previewCsp(opts))}">` +
    '<meta name="referrer" content="no-referrer">';
  const doctype = /^\s*<!doctype[^>]*>/i.exec(html);
  if (!doctype) return meta + html;
  return doctype[0] + meta + html.slice(doctype[0].length);
}

const HTML_EXTENSIONS = new Set(['html', 'htm', 'xhtml']);

export function isHtmlKey(key: string): boolean {
  const name = key.split('/').pop() ?? '';
  const dot = name.lastIndexOf('.');
  return dot > 0 && HTML_EXTENSIONS.has(name.slice(dot + 1).toLowerCase());
}

/** HTML pages are read whole; above this the preview offers the download. */
export const MAX_HTML_PREVIEW = 5 * 1024 * 1024;
