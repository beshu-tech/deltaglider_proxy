import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile keyboard.ts (zero app deps) to an importable data: URL.
const source = await readFile(new URL('../src/keyboard.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ES2020,
    target: ts.ScriptTarget.ES2020,
    importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
  },
  fileName: 'keyboard.ts',
});

// `keyboard.ts` references the DOM `HTMLElement` global via `instanceof`.
// Provide a minimal stand-in so the module loads in Node, and build fake
// "elements" as instances of it.
class FakeHTMLElement {
  constructor({ tagName = 'DIV', isContentEditable = false } = {}) {
    this.tagName = tagName;
    this.isContentEditable = isContentEditable;
  }
}
globalThis.HTMLElement = FakeHTMLElement;

const moduleUrl = `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
const { isTypingTarget, anyOverlayOpen, isCommandCombo } = await import(moduleUrl);

// --- isTypingTarget ----------------------------------------------------------
assert.equal(isTypingTarget(null), false, 'null target is not typing');
assert.equal(isTypingTarget(new FakeHTMLElement({ tagName: 'DIV' })), false, 'plain div is not typing');
assert.equal(isTypingTarget(new FakeHTMLElement({ tagName: 'INPUT' })), true, 'input is typing');
assert.equal(isTypingTarget(new FakeHTMLElement({ tagName: 'TEXTAREA' })), true, 'textarea is typing');
assert.equal(isTypingTarget(new FakeHTMLElement({ tagName: 'SELECT' })), true, 'select is typing');
assert.equal(
  isTypingTarget(new FakeHTMLElement({ tagName: 'DIV', isContentEditable: true })),
  true,
  'contenteditable div is typing',
);
// A non-element EventTarget (e.g. window/document) is not a typing target.
assert.equal(isTypingTarget({}), false, 'non-HTMLElement target is not typing');

// --- isCommandCombo ----------------------------------------------------------
assert.equal(isCommandCombo({ metaKey: true, ctrlKey: false, shiftKey: false, altKey: false }), true, '⌘ bare');
assert.equal(isCommandCombo({ metaKey: false, ctrlKey: true, shiftKey: false, altKey: false }), true, 'Ctrl bare (Win/Linux)');
assert.equal(isCommandCombo({ metaKey: false, ctrlKey: false, shiftKey: false, altKey: false }), false, 'no modifier');
assert.equal(isCommandCombo({ metaKey: true, ctrlKey: false, shiftKey: true, altKey: false }), false, '⌘⇧ excluded (DevTools)');
assert.equal(isCommandCombo({ metaKey: true, ctrlKey: false, shiftKey: false, altKey: true }), false, '⌘⌥ excluded');

// --- anyOverlayOpen (fake document with a minimal querySelector) -------------
function fakeDoc(presentSelectors) {
  const present = new Set(presentSelectors);
  return { querySelector: (sel) => (present.has(sel) ? {} : null) };
}
const MODAL = '.ant-modal-root .ant-modal';
const DRAWER = '.ant-drawer-open';
const DROPDOWN = '.ant-select-dropdown:not(.ant-select-dropdown-hidden)';

assert.equal(anyOverlayOpen(fakeDoc([])), false, 'nothing open');
assert.equal(anyOverlayOpen(fakeDoc([MODAL])), true, 'modal open');
assert.equal(anyOverlayOpen(fakeDoc([DRAWER])), true, 'drawer open');
assert.equal(anyOverlayOpen(fakeDoc([DROPDOWN])), true, 'visible select dropdown open');
assert.equal(anyOverlayOpen(fakeDoc([MODAL, DRAWER, DROPDOWN])), true, 'all open');

console.log('keyboard regression checks passed');
