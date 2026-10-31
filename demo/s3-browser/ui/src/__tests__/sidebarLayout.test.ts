import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  SIDEBAR_DEFAULT_WIDTH,
  SIDEBAR_MAX_WIDTH,
  SIDEBAR_MIN_WIDTH,
  clampSidebarWidth,
  filterBuckets,
  filterTerms,
  groupBucketsByBackend,
  groupKey,
  parseCollapsedGroups,
  parseStoredWidth,
  serializeCollapsedGroups,
  sidebarMaxWidth,
} from '../sidebarLayout';
import type { BucketInfo } from '../types';

const bucket = (name: string, backendName?: string): BucketInfo => ({
  name, creationDate: '', backend: backendName === undefined ? undefined : { backendName },
});

test('the default width is at least 20% wider than the old 250 px', () => {
  assert.ok(SIDEBAR_DEFAULT_WIDTH >= 250 * 1.2);
  assert.ok(SIDEBAR_MIN_WIDTH <= SIDEBAR_DEFAULT_WIDTH && SIDEBAR_DEFAULT_WIDTH <= SIDEBAR_MAX_WIDTH);
});

test('clampSidebarWidth: inside [min, min(max, half the viewport)]', () => {
  assert.equal(clampSidebarWidth(100, 1920), SIDEBAR_MIN_WIDTH);
  assert.equal(clampSidebarWidth(400.6, 1920), 401);
  assert.equal(clampSidebarWidth(5000, 1920), SIDEBAR_MAX_WIDTH);
  assert.equal(clampSidebarWidth(500, 900), 450);
  // A viewport too narrow for the minimum still gets the minimum.
  assert.equal(sidebarMaxWidth(300), SIDEBAR_MIN_WIDTH);
  assert.equal(clampSidebarWidth(500, 300), SIDEBAR_MIN_WIDTH);
  assert.equal(clampSidebarWidth(Number.NaN, 1920), SIDEBAR_DEFAULT_WIDTH);
});

test('parseStoredWidth: a positive integer, else null', () => {
  assert.equal(parseStoredWidth('400'), 400);
  assert.equal(parseStoredWidth(' 400 '), 400);
  for (const raw of [null, '', '0', '-5', '12.5', '1e3', 'wide']) assert.equal(parseStoredWidth(raw), null, String(raw));
});

test('groupBucketsByBackend: null unless the buckets span more than one backend', () => {
  assert.equal(groupBucketsByBackend([]), null);
  assert.equal(groupBucketsByBackend([bucket('a'), bucket('b')]), null);
  assert.equal(groupBucketsByBackend([bucket('a', 'x'), bucket('b', 'x')]), null);
  assert.equal(groupBucketsByBackend([bucket('a', 'x'), bucket('b')]), null);
});

test('groupBucketsByBackend: name order (numeric, any case), unknown last, list order kept', () => {
  const groups = groupBucketsByBackend([
    bucket('z1', 'node10'),
    bucket('u1'),
    bucket('a1', 'Node2'),
    bucket('b1', 'node10'),
    bucket('m1', 'aws-dr'),
  ])!;
  assert.deepEqual(
    groups.map((g) => [g.backend, g.buckets.map((b) => b.name)]),
    [['aws-dr', ['m1']], ['Node2', ['a1']], ['node10', ['z1', 'b1']], [null, ['u1']]],
  );
  assert.equal(groupKey(groups[3]), '');
});

test('collapsed groups round-trip; malformed storage reads as none', () => {
  const keys = new Set(['local-disk', 'aws-dr', '']);
  assert.equal(serializeCollapsedGroups(keys), '["","aws-dr","local-disk"]');
  assert.deepEqual(parseCollapsedGroups(serializeCollapsedGroups(keys)), keys);
  for (const raw of [null, '', '{', '{"a":1}', '"x"']) assert.equal(parseCollapsedGroups(raw).size, 0, String(raw));
  assert.deepEqual(parseCollapsedGroups('["a", 3, null]'), new Set(['a']));
});

test('filterTerms: lower-cased words; blank is no terms', () => {
  assert.deepEqual(filterTerms(''), []);
  assert.deepEqual(filterTerms('   '), []);
  assert.deepEqual(filterTerms(' Rel  DR '), ['rel', 'dr']);
});

test('filterBuckets: every term in the name, any case, list order kept; blank keeps all', () => {
  const list = [bucket('releases-dr', 'aws-dr'), bucket('downloads'), bucket('Releases', 'hetzner-fsn1')];
  assert.deepEqual(filterBuckets(list, ''), list);
  assert.deepEqual(filterBuckets(list, 'REL').map((b) => b.name), ['releases-dr', 'Releases']);
  assert.deepEqual(filterBuckets(list, 'rel dr').map((b) => b.name), ['releases-dr']);
  assert.deepEqual(filterBuckets(list, 'aws'), []);
});
