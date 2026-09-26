import assert from 'node:assert/strict';
import { test } from 'vitest';
import { qk } from '../queries/keys';

// TanStack Query invalidation matches by key PREFIX. If `list()` is a prefix
// of a sibling key, invalidating the list also refetches the sibling (canned
// policies after every user edit; every open drawer's runs/failures after
// every jobs-list poll trigger). `all()` is the explicit root for a broad
// invalidation.
const isPrefix = (a: readonly unknown[], b: readonly unknown[]): boolean =>
  a.length < b.length && a.every((v, i) => v === b[i]);

type KeyFn = (arg: string) => readonly unknown[];
type Family = Record<string, KeyFn>;

function familyKeys(family: Family): Array<[string, readonly unknown[]]> {
  return Object.entries(family)
    .filter(([name]) => name !== 'all')
    .map(([name, fn]) => [name, fn('x')]);
}

test('no key in the users/jobs families is a prefix of a sibling', () => {
  for (const famName of ['users', 'jobs'] as const) {
    const keys = familyKeys(qk[famName]);
    for (const [a, ka] of keys) {
      for (const [b, kb] of keys) {
        assert.ok(!isPrefix(ka, kb), `qk.${famName}.${a}() is a prefix of qk.${famName}.${b}()`);
      }
    }
  }
});

test('list() keys and jobs.all() prefix coverage', () => {
  assert.deepEqual(qk.users.list(), ['users', 'list']);
  assert.deepEqual(qk.jobs.list(), ['jobs', 'list']);
  // The root still covers every jobs key for a deliberate broad refresh.
  for (const [, k] of familyKeys(qk.jobs)) assert.ok(isPrefix(qk.jobs.all(), k));
});
