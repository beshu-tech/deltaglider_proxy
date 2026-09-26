import assert from 'node:assert/strict';
import { test } from 'vitest';
import { folderSizeBound, folderSizeText, folderSizeTitle } from '../folderSize';

test('folderSizeBound: sizes_estimated/truncated decide exact vs atLeast vs about', () => {
  // Round-2 review: `sizes_estimated` / `truncated` were ignored, so an
  // estimated total looked exact.
  assert.equal(folderSizeBound({}), 'exact');
  assert.equal(folderSizeBound({ truncated: true }), 'atLeast');
  // An estimate can over-count (encrypted objects list their ciphertext size),
  // so it is "about", never "at least" — even when the scan was also truncated.
  assert.equal(folderSizeBound({ sizes_estimated: true }), 'about');
  assert.equal(folderSizeBound({ sizes_estimated: true, truncated: true }), 'about');
  // A child row uses its own estimate flag; truncation covers every child.
  assert.equal(folderSizeBound({ sizes_estimated: true }, { sizes_estimated: false }), 'exact');
  assert.equal(folderSizeBound({}, { sizes_estimated: true }), 'about');
  assert.equal(folderSizeBound({ truncated: true }, { sizes_estimated: false }), 'atLeast');
});

test('folderSizeText: bound prefix', () => {
  assert.equal(folderSizeText('3.0 MB', 'exact'), '3.0 MB');
  assert.equal(folderSizeText('3.0 MB', 'atLeast'), '≥ 3.0 MB');
  assert.equal(folderSizeText('3.0 MB', 'about'), '≈ 3.0 MB');
});

test('folderSizeTitle: hover text wording', () => {
  assert.match(folderSizeTitle(1, 'exact'), /^1 file\. Original size/);
  assert.match(folderSizeTitle(2, 'about'), /^2 files\. About this size/);
  assert.match(folderSizeTitle(2, 'atLeast'), /At least this size/);
  assert.doesNotMatch(folderSizeTitle(2, 'exact'), /stored \(compressed\)/);
});
