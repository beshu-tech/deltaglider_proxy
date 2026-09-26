import assert from 'node:assert/strict';
import { test } from 'vitest';
import { isRedactedEmptyAccessYaml } from '../yamlUtils';

test('isRedactedEmptyAccessYaml', () => {
  // "empty" shapes
  assert.equal(isRedactedEmptyAccessYaml('access:'), true);
  assert.equal(isRedactedEmptyAccessYaml('access: {}'), true);
  assert.equal(isRedactedEmptyAccessYaml('access: {   }'), true);

  // leading full-line comments are stripped before the emptiness check
  assert.equal(isRedactedEmptyAccessYaml('# comment\naccess: {}'), true);
  assert.equal(isRedactedEmptyAccessYaml('   # indented\naccess:'), true);
  assert.equal(isRedactedEmptyAccessYaml('# a\n# b\n\naccess: {}\n\n'), true);

  // non-empty / wrong-section / empty-doc shapes
  assert.equal(isRedactedEmptyAccessYaml('access:\n  iam_mode: gui'), false);
  assert.equal(isRedactedEmptyAccessYaml('access:\n  iam_users: []'), false);
  assert.equal(isRedactedEmptyAccessYaml('storage: {}'), false); // wrong section
  assert.equal(isRedactedEmptyAccessYaml(''), false);
  // a `#` mid-value (not at line start) is NOT a comment line, so the access body
  // is not "empty" — this exercises the strip predicate's line-start anchoring.
  assert.equal(isRedactedEmptyAccessYaml('access: val # trailing'), false);
});
