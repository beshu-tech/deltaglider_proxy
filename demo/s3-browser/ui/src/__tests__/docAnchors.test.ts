/**
 * Every intra-docs #anchor link resolves in the product viewer: an in-page
 * "#x" to an id on the same page, and a "page.md#x" to an id on the page that
 * findDocByFilename picks. The ids come from the exact pipeline DocsPage
 * renders (splitMermaid + DOC_REMARK_PLUGINS/DOC_REHYPE_PLUGINS), so a slug
 * rule that drifts from the GitHub one the markdown assumes fails here. The
 * website has the same test in marketing/src/lib/anchors.test.ts.
 */
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import ReactMarkdown from 'react-markdown';
import { expect, test } from 'vitest';
import { buildDocsBundle, findDocByFilename, type DocsPayload } from '../docsBundle';
import { DOC_REHYPE_PLUGINS, DOC_REMARK_PLUGINS, classifyDocHref, splitMermaid } from '../docsMarkdown';

const DOCS_DIR = fileURLToPath(new URL('../../../../../docs/product/', import.meta.url));

async function loadPayload(): Promise<DocsPayload> {
  const manifest = JSON.parse(await readFile(`${DOCS_DIR}manifest.json`, 'utf8')) as DocsPayload['manifest'];
  const docs = await Promise.all(
    manifest.docs.map(async (d) => ({ path: d.path, content: await readFile(`${DOCS_DIR}${d.path}.md`, 'utf8') })),
  );
  return { manifest, docs };
}

/** The HTML DocsPage renders for one doc's markdown (diagrams left out). */
function renderDoc(markdown: string): string {
  return splitMermaid(markdown)
    .filter((s) => s.type === 'text')
    .map((s) =>
      renderToStaticMarkup(
        createElement(ReactMarkdown, { remarkPlugins: DOC_REMARK_PLUGINS, rehypePlugins: DOC_REHYPE_PLUGINS }, s.content),
      ),
    )
    .join('');
}

const decode = (s: string) => s.replace(/&amp;/g, '&').replace(/&quot;/g, '"').replace(/&#x27;/g, "'");

test('every intra-docs #anchor link resolves (product viewer)', async () => {
  const { docs } = buildDocsBundle(await loadPayload());
  expect(docs.length).toBeGreaterThan(0);
  const html = new Map(docs.map((d) => [d.id, renderDoc(d.content)]));
  const ids = new Map([...html].map(([id, h]) => [id, new Set([...h.matchAll(/\bid="([^"]+)"/g)].map((m) => decode(m[1])))]));

  const broken: string[] = [];
  let checked = 0;
  for (const doc of docs) {
    for (const m of (html.get(doc.id) ?? '').matchAll(/<a href="([^"]*)"/g)) {
      const target = classifyDocHref(decode(m[1]));
      if (target.kind === 'external') continue;
      if (target.kind === 'anchor') {
        checked++;
        if (target.anchor && !ids.get(doc.id)?.has(target.anchor)) broken.push(`${doc.filename}: #${target.anchor}`);
        continue;
      }
      if (!target.anchor) continue;
      checked++;
      const hit = findDocByFilename(docs, target.file, doc.filename);
      if (!hit) broken.push(`${doc.filename}: ${target.file} (no such doc)`);
      else if (!ids.get(hit.id)?.has(target.anchor)) broken.push(`${doc.filename}: ${target.file}#${target.anchor}`);
    }
  }
  // The docs carry dozens of anchored links; zero checked means the scan broke.
  expect(checked).toBeGreaterThan(30);
  expect(broken).toEqual([]);
}, 60_000);
