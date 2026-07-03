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

const url = await loadModule('../src/linkifyDocUrl.ts', 'linkifyDocUrl.ts');
const { docsUrlToInAppHref, splitLinkSegments } = await import(url);

// --- docsUrlToInAppHref ------------------------------------------------------
// The doc-link rewrite mirrors docs-imports.ts pathToId flattening (slash → dash).
assert.equal(
  docsUrlToInAppHref('https://deltaglider.com/docs/how-to/backend-capability-validation'),
  '/_/docs/how-to-backend-capability-validation'
);
assert.equal(
  docsUrlToInAppHref('https://www.deltaglider.com/docs/reference/configuration'),
  '/_/docs/reference-configuration'
);
// Non-docs URLs are left alone.
assert.equal(docsUrlToInAppHref('https://deltaglider.com/pricing'), null);
assert.equal(docsUrlToInAppHref('https://example.com/docs/how-to/x'), null);
assert.equal(docsUrlToInAppHref('http://deltaglider.com/docs/how-to/x'), null); // https only

// --- splitLinkSegments -------------------------------------------------------
// The exact 403 message shape guard A emits.
const msg403 =
  "Bucket 'mirror' is replication_target_only: client writes are disabled so " +
  'replication remains the single writer — see ' +
  'https://deltaglider.com/docs/how-to/backend-capability-validation';
const segs = splitLinkSegments(msg403);
assert.equal(segs.length, 2);
assert.equal(segs[0].kind, 'text');
assert.equal(segs[1].kind, 'link');
assert.equal(segs[1].href, '/_/docs/how-to-backend-capability-validation');

// Trailing prose punctuation stays out of the link.
const withDot = splitLinkSegments('see https://deltaglider.com/docs/faq.');
assert.equal(withDot[1].text, 'https://deltaglider.com/docs/faq');
assert.equal(withDot[1].href, '/_/docs/faq');
assert.equal(withDot[2].text, '.');

// Multiple links + non-docs link opens as-is.
const multi = splitLinkSegments(
  'a https://example.com/x b https://deltaglider.com/docs/how-to/upgrade c'
);
assert.deepEqual(
  multi.map((s) => s.kind),
  ['text', 'link', 'text', 'link', 'text']
);
assert.equal(multi[1].href, 'https://example.com/x');
assert.equal(multi[3].href, '/_/docs/how-to-upgrade');

// No links → one text segment; empty string → no segments.
assert.deepEqual(splitLinkSegments('plain warning'), [{ kind: 'text', text: 'plain warning' }]);
assert.deepEqual(splitLinkSegments(''), []);

// A URL inside quotes/parens doesn't swallow the closer.
const quoted = splitLinkSegments('(see "https://deltaglider.com/docs/faq")');
assert.equal(quoted.find((s) => s.kind === 'link').text, 'https://deltaglider.com/docs/faq');

console.log('linkify-regression-test: all assertions passed');
