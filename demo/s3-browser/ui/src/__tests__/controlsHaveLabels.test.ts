// review4 frontend-3: a form control needs an accessible name. FormField
// wires its label to its single child; a control outside FormField must
// carry aria-label, aria-labelledby or an id (for a <label htmlFor>).
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import ts from 'typescript';
import { expect, test } from 'vitest';

const CONTROLS = new Set([
  'Input', 'Input.Password', 'Input.TextArea', 'InputNumber', 'Switch', 'Select',
  'Checkbox', 'MaskedSecretInput', 'SimpleAutoComplete',
]);
const NAMING_ATTRS = new Set(['aria-label', 'aria-labelledby', 'id']);

async function sources(dir: string): Promise<string[]> {
  const out: string[] = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    if (e.name === '__tests__' || e.name === 'test') continue;
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...(await sources(p)));
    else if (e.name.endsWith('.tsx') && !e.name.endsWith('.test.tsx') && e.name !== 'storyboard.tsx') out.push(p);
  }
  return out;
}

/** True when the nearest enclosing JSX element is a FormField. */
function insideFormField(node: ts.Node, sf: ts.SourceFile): boolean {
  for (let p = node.parent; p && !ts.isSourceFile(p); p = p.parent) {
    if (ts.isJsxElement(p) && p !== node) return p.openingElement.tagName.getText(sf) === 'FormField';
  }
  return false;
}

function unlabelledControls(file: string, text: string): string[] {
  const sf = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const hits: string[] = [];
  const visit = (node: ts.Node) => {
    if (ts.isJsxSelfClosingElement(node) || ts.isJsxOpeningElement(node)) {
      const name = node.tagName.getText(sf);
      if (CONTROLS.has(name)) {
        const element = ts.isJsxOpeningElement(node) ? node.parent : node;
        const named = node.attributes.properties.some(
          (p) => ts.isJsxSpreadAttribute(p) || NAMING_ATTRS.has(p.name.getText(sf)),
        );
        // A Checkbox's own children are its label.
        const textChildren = name === 'Checkbox' && ts.isJsxElement(element) && element.children.length > 0;
        if (!named && !textChildren && !insideFormField(element, sf)) {
          hits.push(`${file}:${sf.getLineAndCharacterOfPosition(node.getStart()).line + 1} <${name}>`);
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sf);
  return hits;
}

test('every form control has an accessible name', async () => {
  const root = new URL('../', import.meta.url).pathname;
  const hits: string[] = [];
  for (const f of await sources(root)) {
    hits.push(...unlabelledControls(f.replace(root, 'src/'), await readFile(f, 'utf8')));
  }
  expect(hits).toEqual([]);
});
