// Regression: a change inside one rule showed the WHOLE rules array, before
// and after, truncated at the same prefix: the change was invisible.
import { expect, test } from 'vitest';
import { expandDiff } from '../applyDiff';

const rule = (name: string, over: Record<string, unknown> = {}) => ({
  name,
  enabled: true,
  source: { bucket: 'releases', prefix: '' },
  destination: { bucket: 'downloads', prefix: '' },
  replicate_deletes: false,
  ...over,
});

test('one changed field of one rule is one line', () => {
  const before = [rule('a'), rule('b')];
  const after = [rule('a'), rule('b', { replicate_deletes: true, source: { bucket: 'releases', prefix: 'v2' } })];
  expect(expandDiff([['replication.rules', { before, after }]])).toEqual([
    { path: 'replication.rules["b"].replicate_deletes', kind: 'changed', before: false, after: true },
    { path: 'replication.rules["b"].source.prefix', kind: 'changed', before: '', after: 'v2' },
  ]);
});

test('added and removed rules are named', () => {
  const lines = expandDiff([['lifecycle.rules', { before: [rule('old')], after: [rule('new')] }]]);
  expect(lines.map((l) => [l.kind, l.path])).toEqual([
    ['added', 'lifecycle.rules["new"]'],
    ['removed', 'lifecycle.rules["old"]'],
  ]);
  // The first rule ever: before is null.
  expect(expandDiff([['lifecycle.rules', { before: null, after: [rule('x')] }]])[0].kind).toBe('added');
});

test('reordered rules and plain values pass through', () => {
  expect(expandDiff([['r', { before: [rule('a'), rule('b')], after: [rule('b'), rule('a')] }]])).toEqual([
    { path: 'r', kind: 'changed', before: ['a', 'b'], after: ['b', 'a'] },
  ]);
  expect(expandDiff([['lifecycle.enabled', { before: false, after: true }]])).toEqual([
    { path: 'lifecycle.enabled', kind: 'changed', before: false, after: true },
  ]);
  expect(expandDiff([['x.globs', { before: ['a'], after: ['b'] }]])[0].path).toBe('x.globs');
});
