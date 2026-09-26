/** src/docsBundle.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { buildDocsBundle, findDocByFilename, type DocsPayload } from '../docsBundle';

test('buildDocsBundle', () => {
  const payload: DocsPayload = {
    manifest: {
      groups: [
        { id: 'Start here', tagline: 'Lessons.' },
        { id: 'Reference', tagline: 'Facts.' },
      ],
      docs: [
        { path: 'Reference/Metrics', group: 'Reference', order: 20 },
        { path: 'readme', group: 'Start here', order: 0 },
        { path: 'reference/missing-on-disk', group: 'Reference', order: 30 },
      ],
    },
    docs: [
      { path: 'readme', content: 'intro line\n# DeltaGlider Proxy\ntext' },
      { path: 'Reference/Metrics', content: '# Metrics\n## HTTP requests' },
    ],
  };
  const bundle = buildDocsBundle(payload);
  assert.deepEqual(bundle.groups, ['Start here', 'Reference']);
  assert.deepEqual(bundle.taglines, { 'Start here': 'Lessons.', Reference: 'Facts.' });
  // Manifest order is preserved; a manifest entry without content is skipped, not thrown.
  assert.deepEqual(
    bundle.docs.map((d) => [d.id, d.title, d.filename, d.group, d.order]),
    [
      // ids are flattened + lower-cased (slash → dash); filenames keep the path
      ['reference-metrics', 'Metrics', 'Reference/Metrics.md', 'Reference', 20],
      ['readme', 'DeltaGlider Proxy', 'readme.md', 'Start here', 0],
    ],
  );
  assert.equal(bundle.docs[1].content, payload.docs[0].content);
});

test('findDocByFilename', () => {
  const bundle = buildDocsBundle({
    manifest: {
      groups: [
        { id: 'Start here', tagline: 'Lessons.' },
        { id: 'Reference', tagline: 'Facts.' },
      ],
      docs: [
        { path: 'Reference/Metrics', group: 'Reference', order: 20 },
        { path: 'readme', group: 'Start here', order: 0 },
        { path: 'reference/missing-on-disk', group: 'Reference', order: 30 },
      ],
    },
    docs: [
      { path: 'readme', content: 'intro line\n# DeltaGlider Proxy\ntext' },
      { path: 'Reference/Metrics', content: '# Metrics\n## HTTP requests' },
    ],
  });
  const docs = bundle.docs;
  assert.equal(findDocByFilename(docs, 'readme.md')?.id, 'readme');
  assert.equal(findDocByFilename(docs, '../readme.md#install')?.id, 'readme');
  assert.equal(findDocByFilename(docs, './Reference/Metrics.md?x=1')?.id, 'reference-metrics');
  assert.equal(findDocByFilename(docs, '../../Reference/Metrics.md')?.id, 'reference-metrics');
  assert.equal(findDocByFilename(docs, 'nope.md'), undefined);
  assert.equal(findDocByFilename(docs, 'https://example.com/readme.md'), undefined);
  assert.equal(findDocByFilename(docs, 'Reference/Metrics'), undefined, 'links must carry .md');
  assert.equal(findDocByFilename([], 'readme.md'), undefined);
});

test('folder-relative links (the most common shape in docs/product)', () => {
  const tree = buildDocsBundle({
    manifest: {
      groups: [{ id: 'g', tagline: '' }],
      docs: [
        { path: 'how-to/go-to-production', group: 'g', order: 1 },
        { path: 'how-to/serve-tls', group: 'g', order: 2 },
        { path: 'reference/metrics', group: 'g', order: 3 },
        { path: 'faq', group: 'g', order: 4 },
      ],
    },
    docs: [
      { path: 'how-to/go-to-production', content: '# Go' },
      { path: 'how-to/serve-tls', content: '# TLS' },
      { path: 'reference/metrics', content: '# Metrics' },
      { path: 'faq', content: '# FAQ' },
    ],
  }).docs;
  const fromHowTo = 'how-to/go-to-production.md';
  assert.equal(findDocByFilename(tree, 'serve-tls.md', fromHowTo)?.id, 'how-to-serve-tls', 'same-folder sibling');
  assert.equal(findDocByFilename(tree, './serve-tls.md#anchor', fromHowTo)?.id, 'how-to-serve-tls');
  assert.equal(findDocByFilename(tree, '../reference/metrics.md', fromHowTo)?.id, 'reference-metrics', 'sibling folder');
  assert.equal(findDocByFilename(tree, '../faq.md', fromHowTo)?.id, 'faq', 'back to top');
  assert.equal(findDocByFilename(tree, 'reference/metrics.md', 'faq.md')?.id, 'reference-metrics', 'from top into a folder');
  assert.equal(findDocByFilename(tree, 'serve-tls.md')?.id, 'how-to-serve-tls', 'no context: bare-filename fallback');
  assert.equal(findDocByFilename(tree, 'nope.md', fromHowTo), undefined);
  assert.equal(findDocByFilename(tree, 'https://example.com/serve-tls.md', fromHowTo), undefined, 'absolute URLs are not docs');
});
