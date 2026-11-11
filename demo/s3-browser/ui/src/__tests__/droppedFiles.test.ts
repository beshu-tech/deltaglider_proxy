import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'vitest';
import { collectDroppedFiles, skippedDropMessage, type EntryLike, type ReaderLike, type DataTransferLike } from '../droppedFiles';

// ── Mock builders (callback-based, like the real FileSystem*Entry API) ──────
const mkFile = (name: string, content = 'x'): File => new File([content], name, { type: 'text/plain' });

function fileEntry(name: string, file: File = mkFile(name)): EntryLike {
  return { isFile: true, isDirectory: false, name, file: (ok) => ok(file) };
}
function brokenFileEntry(name: string): EntryLike {
  return { isFile: true, isDirectory: false, name, file: (_ok, err) => err(new Error('NotReadableError')) };
}
/** Directory entry whose reader yields `batches` (arrays of entries), then []. */
function dirEntry(name: string, batches: EntryLike[][]): EntryLike {
  return {
    isFile: false,
    isDirectory: true,
    name,
    createReader: (): ReaderLike => {
      let i = 0;
      return { readEntries: (ok) => ok(i < batches.length ? batches[i++] : []) };
    },
  };
}
const itemFor = (entry: EntryLike | null) => ({ webkitGetAsEntry: () => entry });

test('no entry API at all → falls back to dt.files verbatim', async () => {
  const a = mkFile('a.txt');
  const out = await collectDroppedFiles({ files: [a] });
  assert.deepEqual(out, [a], 'no items → dt.files fallback');
});

test('synthetic DataTransfer (items exist, getAsEntry → null) → fallback', async () => {
  const a = mkFile('a.txt');
  const out = await collectDroppedFiles({
    items: [{ webkitGetAsEntry: () => null, getAsFile: () => a }],
    files: [a],
  });
  assert.deepEqual(out, [a], 'all-null entries → dt.files fallback');
});

test('top-level plain file entry → original File kept, name unchanged', async () => {
  const a = mkFile('a.txt');
  const out = await collectDroppedFiles({ items: [itemFor(fileEntry('a.txt', a))], files: [a] });
  assert.equal(out.length, 1);
  assert.equal(out[0], a, 'top-level file is not re-wrapped');
});

test('dropped FOLDER with nesting → real files with folder-relative path names', async () => {
  // ducky/ { 66000, sub/ { deep.bin } }   ← the reported repro shape
  const out = await collectDroppedFiles({
    items: [
      itemFor(
        dirEntry('ducky', [[fileEntry('66000'), dirEntry('sub', [[fileEntry('deep.bin')]])]]),
      ),
    ],
    files: [mkFile('ducky', 'directory-metadata-garbage')], // what dt.files lies with
  });
  const names = out.map((f) => f.name).sort();
  assert.deepEqual(names, ['ducky/66000', 'ducky/sub/deep.bin'], 'folder walked into real files with relative paths');
  // The unreadable directory pseudo-file from dt.files must NOT be included.
  assert.ok(!names.includes('ducky'), 'directory pseudo-file is not enqueued');
});

test('readEntries batching: two non-empty batches then the empty terminator', async () => {
  const batch1 = [fileEntry('one.txt')];
  const batch2 = [fileEntry('two.txt')];
  const out = await collectDroppedFiles({
    items: [itemFor(dirEntry('d', [batch1, batch2]))],
    files: [],
  });
  assert.deepEqual(out.map((f) => f.name).sort(), ['d/one.txt', 'd/two.txt'], 'reader drained across batches');
});

test('unreadable file inside a folder is skipped; siblings survive', async () => {
  // (The helper console.warns on the skip — capture it instead of spamming CI.)
  const warns: unknown[] = [];
  const realWarn = console.warn;
  console.warn = (...args: unknown[]) => {
    warns.push(args[0]);
  };
  try {
    const out = await collectDroppedFiles({
      items: [itemFor(dirEntry('d', [[brokenFileEntry('bad'), fileEntry('good.txt')]]))],
      files: [],
    });
    assert.deepEqual(out.map((f) => f.name), ['d/good.txt'], 'broken file skipped, sibling kept');
    assert.ok(warns.some((w) => String(w).includes('d/bad')), 'skip is warned with the file path');
  } finally {
    console.warn = realWarn;
  }
});

test('mixed drop: one real folder entry + one null-entry item with getAsFile', async () => {
  const loose = mkFile('loose.txt');
  const dt: DataTransferLike = {
    items: [
      itemFor(dirEntry('d', [[fileEntry('in.txt')]])),
      { webkitGetAsEntry: () => null, getAsFile: () => loose },
    ],
    files: [loose],
  };
  const out = await collectDroppedFiles(dt);
  assert.deepEqual(out.map((f) => f.name).sort(), ['d/in.txt', 'loose.txt'], 'entry + loose item both collected');
});

// F62 (epic #146): a file or folder that could not be read was skipped with a
// console.warn only: the user saw a partial upload and no word why. The
// caller now learns the skipped paths and tells the user.
test('skipped paths are reported to the caller, not only to the console', async () => {
  const realWarn = console.warn;
  console.warn = () => {};
  try {
    const skipped: string[][] = [];
    const out = await collectDroppedFiles(
      {
        items: [itemFor(dirEntry('d', [[brokenFileEntry('bad'), fileEntry('good.txt')]])), itemFor(brokenFileEntry('top.bin'))],
        files: [],
      },
      (paths) => skipped.push(paths),
    );
    assert.deepEqual(out.map((f) => f.name), ['d/good.txt']);
    assert.deepEqual(skipped, [['d/bad', 'top.bin']], 'one report, with every skipped path in drop order');
  } finally {
    console.warn = realWarn;
  }
});

test('a drop with nothing skipped reports nothing', async () => {
  const skipped: string[][] = [];
  await collectDroppedFiles({ items: [itemFor(fileEntry('a.txt'))], files: [] }, (p) => skipped.push(p));
  assert.deepEqual(skipped, []);
});

test('the message says how many were skipped and names the first ones', () => {
  assert.equal(
    skippedDropMessage(['d/bad']),
    '1 dropped item could not be read and was not added: d/bad.',
  );
  assert.equal(
    skippedDropMessage(['a', 'b', 'c', 'd', 'e']),
    '5 dropped items could not be read and were not added: a, b, c and 2 more.',
  );
});

test('both drop handlers tell the user what was skipped', () => {
  for (const file of ['components/DropZone.tsx', 'components/UploadPage.tsx']) {
    const text = readFileSync(join(__dirname, '..', file), 'utf8');
    assert.match(text, /collectDroppedFiles\([^)]*,\s*\(?\w+\)?\s*=>/s, `${file} passes a skipped-paths handler`);
    assert.match(text, /skippedDropMessage\(/, `${file} words the skip`);
  }
});
