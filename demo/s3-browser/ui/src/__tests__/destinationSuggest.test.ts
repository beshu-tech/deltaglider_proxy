import assert from 'node:assert/strict';
import { test } from 'vitest';
import { splitDestinationInput, filterFolderOptions } from '../destinationSuggest';

test('splitDestinationInput', () => {
  assert.deepEqual(splitDestinationInput(''), { parent: '', tail: '' }, 'empty → bucket root');
  assert.deepEqual(splitDestinationInput('tir'), { parent: '', tail: 'tir' }, 'first segment being typed');
  assert.deepEqual(splitDestinationInput('tirso/'), { parent: 'tirso/', tail: '' }, 'complete segment → list inside');
  assert.deepEqual(splitDestinationInput('tirso/su'), { parent: 'tirso/', tail: 'su' }, 'nested segment being typed');
  assert.deepEqual(splitDestinationInput('a/b/c'), { parent: 'a/b/', tail: 'c' }, 'deep path');
  assert.deepEqual(splitDestinationInput('/tirso'), { parent: '', tail: 'tirso' }, 'leading slash stripped');
  assert.deepEqual(splitDestinationInput('a//b'), { parent: 'a/', tail: 'b' }, 'duplicate slashes collapsed');
});

test('filterFolderOptions', () => {
  const folders = ['tirso/', 'temp/', 'demo/', '66000/'];
  assert.deepEqual(filterFolderOptions(folders, '', ''), ['66000/', 'demo/', 'temp/', 'tirso/'], 'no tail → all, sorted');
  assert.deepEqual(filterFolderOptions(folders, '', 't'), ['temp/', 'tirso/'], 'tail prefix-matches the segment');
  assert.deepEqual(filterFolderOptions(folders, '', 'TIR'), ['tirso/'], 'case-insensitive match');
  assert.deepEqual(filterFolderOptions(folders, '', 'zzz'), [], 'no match → empty');
  // Nested level: folders come back as full prefixes under the parent.
  const nested = ['tirso/sub/', 'tirso/songs/'];
  assert.deepEqual(filterFolderOptions(nested, 'tirso/', 's'), ['tirso/songs/', 'tirso/sub/'], 'nested match strips parent before comparing');
  assert.deepEqual(filterFolderOptions(nested, 'tirso/', 'su'), ['tirso/sub/'], 'nested narrower match');
});
