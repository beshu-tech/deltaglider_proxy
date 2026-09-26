/**
 * Regression guard for MODAL-DEAD-BACK-ENTRY: the inspector's download/share
 * modal pushed a history entry, but closing it without Back (object change,
 * button) did not pop it, so the next Back landed on the dead entry and did
 * nothing. useBackClosesModal pops its own entry — but only while that entry
 * is still current, so it never undoes a navigation pushed on top.
 */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { pushModalEntry, popModalEntryIfTop } from '../hooks/modalHistory';

interface FakeEntry {
  state: unknown;
  url: string;
}

interface FakeHistory {
  entries: FakeEntry[];
  index: number;
  readonly state: unknown;
  readonly url: string;
  pushState(state: unknown, _t: string, url: string): void;
  back(): void;
}

// Minimal History fake: a stack of { state, url } and an index.
function fakeHistory(initialState: unknown = null): FakeHistory {
  const h: FakeHistory = {
    entries: [{ state: initialState, url: '/_/browse/b?object=a' }],
    index: 0,
    get state() { return h.entries[h.index].state; },
    get url() { return h.entries[h.index].url; },
    pushState(state, _t, url) {
      h.entries = h.entries.slice(0, h.index + 1);
      h.entries.push({ state, url });
      h.index += 1;
    },
    back() { if (h.index > 0) h.index -= 1; },
  };
  return h;
}

test('open then programmatic close pops the entry: the next Back leaves the page state', () => {
  const h = fakeHistory({ keep: 1 });
  pushModalEntry(h, 'm1', h.url);
  assert.equal(h.entries.length, 2);
  assert.deepEqual(h.state, { keep: 1, dgModal: 'm1' }, 'existing state keys are kept');
  assert.equal(popModalEntryIfTop(h, 'm1'), true);
  assert.equal(h.index, 0, 'back to the entry under the modal');
});

test('a navigation pushed on top of the modal entry is never undone', () => {
  const h = fakeHistory();
  pushModalEntry(h, 'm2', h.url);
  h.pushState(null, '', '/_/browse/b?object=other');
  assert.equal(popModalEntryIfTop(h, 'm2'), false);
  assert.equal(h.url, '/_/browse/b?object=other', 'navigation kept');
});

test("a different modal's entry is not popped", () => {
  const h = fakeHistory();
  pushModalEntry(h, 'm3', h.url);
  assert.equal(popModalEntryIfTop(h, 'other'), false);
  assert.equal(h.index, 1);
});
