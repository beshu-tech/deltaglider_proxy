import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile a TS module to an importable data: URL (no bundler).
async function loadModule(relPath, fileName) {
  const url = new URL(relPath, import.meta.url);
  const source = await readFile(url, 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ES2020,
      target: ts.ScriptTarget.ES2020,
    },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const url = await loadModule('../src/docsBundle.ts', 'docsBundle.ts');
const { buildDocsBundle, findDocByFilename } = await import(url);

// --- buildDocsBundle --------------------------------------------------------
const payload = {
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

// --- findDocByFilename ------------------------------------------------------
const docs = bundle.docs;
assert.equal(findDocByFilename(docs, 'readme.md')?.id, 'readme');
assert.equal(findDocByFilename(docs, '../readme.md#install')?.id, 'readme');
assert.equal(findDocByFilename(docs, './Reference/Metrics.md?x=1')?.id, 'reference-metrics');
assert.equal(findDocByFilename(docs, '../../Reference/Metrics.md')?.id, 'reference-metrics');
assert.equal(findDocByFilename(docs, 'nope.md'), undefined);
assert.equal(findDocByFilename(docs, 'https://example.com/readme.md'), undefined);
assert.equal(findDocByFilename(docs, 'Reference/Metrics'), undefined, 'links must carry .md');
assert.equal(findDocByFilename([], 'readme.md'), undefined);

console.log('docs-bundle regression test: OK');
