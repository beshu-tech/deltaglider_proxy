import assert from 'node:assert/strict';
import { test } from 'vitest';
import type { FilterPreview } from '../adminApi/jobs';
import { filterSummary, ruleFolderPrefix, splitAtPrefix, verdictText } from '../ruleFilterPreview';

const base: FilterPreview = {
  sampled: 1000,
  truncated: true,
  included: 980,
  excluded: 15,
  not_included: 5,
  internal: 0,
  globs: [],
  keys: [],
};

test('the summary says what the filters do to the sample', () => {
  assert.equal(
    filterSummary(base, 'beshu/ror/', 'replicate'),
    'Of the first 1,000 objects under beshu/ror/, 980 replicate, 15 are excluded and 5 match no include glob.',
  );
  assert.equal(
    filterSummary({ ...base, sampled: 3, truncated: false, included: 3, excluded: 0, not_included: 0 }, 'b/', 'replicate'),
    'Of the 3 objects under b/, 3 replicate.',
  );
  assert.equal(
    filterSummary({ ...base, sampled: 0, truncated: false }, 'b/', 'replicate'),
    'No objects under b/ yet. The filters apply to objects as they arrive.',
  );
});

test('a verdict names the glob that decided', () => {
  assert.equal(verdictText('excluded', '*.tmp', 'replicate'), 'Excluded by *.tmp');
  assert.equal(verdictText('included', '*.zip', 'replicate'), 'Replicates: matches *.zip');
  assert.equal(verdictText('included', undefined, 'are candidates'), 'Is a candidate');
  assert.equal(verdictText('not_included', undefined, 'replicate'), 'Matches no include glob');
});

test('the prefix as the proxy reads it, and the key split at it', () => {
  assert.equal(ruleFolderPrefix(''), '');
  assert.equal(ruleFolderPrefix('/ror//builds'), 'ror/builds/');
  assert.deepEqual(splitAtPrefix('ror/a.zip', 'ror/'), ['ror/', 'a.zip']);
  assert.deepEqual(splitAtPrefix('other/a.zip', 'ror/'), ['', 'other/a.zip']);
});
