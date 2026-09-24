import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Regression guard for MODAL-DEAD-BACK-ENTRY: the inspector's download/share
// modal pushed a history entry, but closing it without Back (object change,
// button) did not pop it, so the next Back landed on the dead entry and did
// nothing. useBackClosesModal pops its own entry — but only while that entry
// is still current, so it never undoes a navigation pushed on top.

const source = await readFile(new URL('../src/hooks/modalHistory.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'modalHistory.ts',
});
const { pushModalEntry, popModalEntryIfTop } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

// Minimal History fake: a stack of { state, url } and an index.
function fakeHistory(initialState = null) {
  const h = {
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

// open → programmatic close pops the entry: the next Back leaves the page state.
{
  const h = fakeHistory({ keep: 1 });
  pushModalEntry(h, 'm1', h.url);
  assert.equal(h.entries.length, 2);
  assert.deepEqual(h.state, { keep: 1, dgModal: 'm1' }, 'existing state keys are kept');
  assert.equal(popModalEntryIfTop(h, 'm1'), true);
  assert.equal(h.index, 0, 'back to the entry under the modal');
}

// a navigation pushed on top of the modal entry is never undone
{
  const h = fakeHistory();
  pushModalEntry(h, 'm2', h.url);
  h.pushState(null, '', '/_/browse/b?object=other');
  assert.equal(popModalEntryIfTop(h, 'm2'), false);
  assert.equal(h.url, '/_/browse/b?object=other', 'navigation kept');
}

// a different modal's entry is not popped
{
  const h = fakeHistory();
  pushModalEntry(h, 'm3', h.url);
  assert.equal(popModalEntryIfTop(h, 'other'), false);
  assert.equal(h.index, 1);
}

console.log('modal-history regression checks passed');
