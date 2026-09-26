import assert from 'node:assert/strict';
import { test } from 'vitest';
import { nextUniqueRuleName } from '../components/ruleNames';

test('nextUniqueRuleName: base-1 on an empty list', () => {
  // Empty list -> base-1 (length 0 + 1).
  assert.equal(nextUniqueRuleName([], 'rule'), 'rule-1');
  assert.equal(nextUniqueRuleName([], 'expire-old'), 'expire-old-1');
});

test('nextUniqueRuleName: starts at length + 1, bumps on collision', () => {
  // Non-colliding: starts at length + 1.
  assert.equal(nextUniqueRuleName([{ name: 'a' }, { name: 'b' }], 'rule'), 'rule-3');

  // Natural index (length 3 -> rule-4) is free even though rule-1/rule-2 exist.
  assert.equal(
    nextUniqueRuleName([{ name: 'rule-1' }, { name: 'rule-2' }, { name: 'x' }], 'rule'),
    'rule-4',
  );
  // Natural index collides -> bump: length 2 -> rule-3 taken -> rule-4.
  assert.equal(
    nextUniqueRuleName([{ name: 'rule-3' }, { name: 'rule-1' }], 'rule'),
    'rule-4',
  );
  // length 1, natural index rule-2 is taken -> bumps to rule-3.
  assert.equal(nextUniqueRuleName([{ name: 'rule-2' }], 'rule'), 'rule-3');
  // length 1, natural index rule-2 is free -> rule-2 (no bump).
  assert.equal(nextUniqueRuleName([{ name: 'rule-1' }], 'rule'), 'rule-2');
  // length 2 (=> start rule-3) but rule-3 taken -> rule-4.
  assert.equal(
    nextUniqueRuleName([{ name: 'rule-3' }, { name: 'z' }], 'rule'),
    'rule-4',
  );

  // Matches the historical lifecycle base (length 1, natural -2 free).
  assert.equal(
    nextUniqueRuleName([{ name: 'expire-old-1' }], 'expire-old'),
    'expire-old-2',
  );
});
