import assert from 'node:assert/strict';
import { test } from 'vitest';
import { isTypingTarget, anyOverlayOpen, isCommandCombo, activateOnKey } from '../keyboard';

// `keyboard.ts` references the DOM `HTMLElement` global via `instanceof`.
// This file runs in the plain-Node vitest project (no jsdom), so provide a
// minimal stand-in before any test calls into the module. `isTypingTarget`
// only looks up `HTMLElement` at CALL time (inside the function body), so
// this stub only needs to run before the tests below execute — module-level
// statements run in order regardless of where the (hoisted) import sits.
class FakeHTMLElement {
  tagName: string;
  isContentEditable: boolean;
  constructor({ tagName = 'DIV', isContentEditable = false }: { tagName?: string; isContentEditable?: boolean } = {}) {
    this.tagName = tagName;
    this.isContentEditable = isContentEditable;
  }
}
(globalThis as unknown as { HTMLElement: typeof FakeHTMLElement }).HTMLElement = FakeHTMLElement;

test('isTypingTarget: input/textarea/select/contenteditable are typing targets', () => {
  assert.equal(isTypingTarget(null), false, 'null target is not typing');
  assert.equal(isTypingTarget(new FakeHTMLElement({ tagName: 'DIV' }) as unknown as EventTarget), false, 'plain div is not typing');
  assert.equal(isTypingTarget(new FakeHTMLElement({ tagName: 'INPUT' }) as unknown as EventTarget), true, 'input is typing');
  assert.equal(isTypingTarget(new FakeHTMLElement({ tagName: 'TEXTAREA' }) as unknown as EventTarget), true, 'textarea is typing');
  assert.equal(isTypingTarget(new FakeHTMLElement({ tagName: 'SELECT' }) as unknown as EventTarget), true, 'select is typing');
  assert.equal(
    isTypingTarget(new FakeHTMLElement({ tagName: 'DIV', isContentEditable: true }) as unknown as EventTarget),
    true,
    'contenteditable div is typing',
  );
  // A non-element EventTarget (e.g. window/document) is not a typing target.
  assert.equal(isTypingTarget({} as EventTarget), false, 'non-HTMLElement target is not typing');
});

test('isCommandCombo: accepts bare ⌘/Ctrl, excludes Shift/Alt combos', () => {
  assert.equal(isCommandCombo({ metaKey: true, ctrlKey: false, shiftKey: false, altKey: false }), true, '⌘ bare');
  assert.equal(isCommandCombo({ metaKey: false, ctrlKey: true, shiftKey: false, altKey: false }), true, 'Ctrl bare (Win/Linux)');
  assert.equal(isCommandCombo({ metaKey: false, ctrlKey: false, shiftKey: false, altKey: false }), false, 'no modifier');
  assert.equal(isCommandCombo({ metaKey: true, ctrlKey: false, shiftKey: true, altKey: false }), false, '⌘⇧ excluded (DevTools)');
  assert.equal(isCommandCombo({ metaKey: true, ctrlKey: false, shiftKey: false, altKey: true }), false, '⌘⌥ excluded');
});

test('anyOverlayOpen: probes for AntD modal/drawer/dropdown/menu presence', () => {
  function fakeDoc(presentSelectors: string[]): Document {
    const present = new Set(presentSelectors);
    return { querySelector: (sel: string) => (present.has(sel) ? {} : null) } as unknown as Document;
  }
  const MODAL = '.ant-modal-root .ant-modal';
  const DRAWER = '.ant-drawer-open';
  const DROPDOWN = '.ant-select-dropdown:not(.ant-select-dropdown-hidden)';

  assert.equal(anyOverlayOpen(fakeDoc([])), false, 'nothing open');
  assert.equal(anyOverlayOpen(fakeDoc([MODAL])), true, 'modal open');
  assert.equal(anyOverlayOpen(fakeDoc([DRAWER])), true, 'drawer open');
  assert.equal(anyOverlayOpen(fakeDoc([DROPDOWN])), true, 'visible select dropdown open');
  assert.equal(anyOverlayOpen(fakeDoc([MODAL, DRAWER, DROPDOWN])), true, 'all open');
  // Escape that closes a menu (bucket row "⋯", account menu) went up a folder too.
  assert.equal(anyOverlayOpen(fakeDoc(['.ant-dropdown:not(.ant-dropdown-hidden)'])), true, 'row menu open');
  assert.equal(anyOverlayOpen(fakeDoc(['.account-menu-panel'])), true, 'account menu open');
});

test('activateOnKey: Enter/Space activate the row itself, never a descendant', () => {
  // Issue #92 review: Enter on a row's "⋯" button opened the job drawer, because
  // the row handled Enter for every descendant.
  let runs = 0;
  let prevented = 0;
  const row = {} as EventTarget;
  const child = {} as EventTarget;
  const ev = (key: string, target: EventTarget) => ({
    key,
    target,
    currentTarget: row,
    preventDefault: () => {
      prevented++;
    },
  });
  const handler = activateOnKey(() => {
    runs++;
  });
  handler(ev('Enter', row));
  handler(ev(' ', row));
  assert.equal(runs, 2, 'Enter and Space on the row itself activate it');
  assert.equal(prevented, 2, 'the row suppresses page scroll on Space');
  handler(ev('Enter', child));
  handler(ev(' ', child));
  assert.equal(runs, 2, 'keys from a descendant never activate the row');
  assert.equal(prevented, 2, 'a descendant keeps its default key behaviour');
  handler(ev('a', row));
  assert.equal(runs, 2, 'other keys do nothing');
});
