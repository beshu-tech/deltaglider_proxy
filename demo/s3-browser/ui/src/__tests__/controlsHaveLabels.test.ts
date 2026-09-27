// review4 frontend-3: a form control needs an accessible name. FormField
// wires its label to its single child; a control outside FormField must
// carry aria-label, aria-labelledby or an id (for a <label htmlFor>).
// BASELINE holds the unlabelled controls that predate this guard: the
// count per file may only go down.
import { readFile, readdir } from 'node:fs/promises';
import { join } from 'node:path';
import ts from 'typescript';
import { expect, test } from 'vitest';

const CONTROLS = new Set([
  'Input', 'Input.Password', 'Input.TextArea', 'InputNumber', 'Switch', 'Select',
  'Checkbox', 'MaskedSecretInput', 'SimpleAutoComplete',
]);
const NAMING_ATTRS = new Set(['aria-label', 'aria-labelledby', 'id']);

const BASELINE: Record<string, number> = {
  'src/components/AuditLogPanel.tsx': 1,
  'src/components/AuthenticationPanel.tsx': 1,
  'src/components/BackendEncryptionEditor.tsx': 2,
  'src/components/BackendsPanel.tsx': 2,
  'src/components/BucketCard.tsx': 6,
  'src/components/BucketPrefixInput.tsx': 2,
  'src/components/CommandPalette.tsx': 1,
  'src/components/ConditionPrefixInput.tsx': 1,
  'src/components/ConnectPage.tsx': 6,
  'src/components/CopySectionYamlButton.tsx': 1,
  'src/components/DeltaEfficiencyPanel.tsx': 2,
  'src/components/DestinationPickerModal.tsx': 2,
  'src/components/DocSearch.tsx': 1,
  'src/components/EventOutboxPanel.tsx': 3,
  'src/components/FullIamYamlModal.tsx': 2,
  'src/components/GlobListTextArea.tsx': 1,
  'src/components/GroupsPanel.tsx': 2,
  'src/components/InspectorPanel.tsx': 1,
  'src/components/LogsPanel.tsx': 4,
  'src/components/MappingRuleRow.tsx': 5,
  'src/components/MasterDetailPanel.tsx': 1,
  'src/components/MigrateBucketModal.tsx': 2,
  'src/components/ObjectTable.tsx': 1,
  'src/components/PasswordChangeCard.tsx': 2,
  'src/components/PermissionEditor.tsx': 1,
  'src/components/PrefixListEditor.tsx': 1,
  'src/components/ResourcePatternInput.tsx': 1,
  'src/components/SessionsPanel.tsx': 1,
  'src/components/SlackConnectorCard.tsx': 4,
  'src/components/TracePanel.tsx': 1,
  'src/components/UserForm.tsx': 3,
  'src/components/WebhookDeliveryPanel.tsx': 3,
  'src/components/YamlImportExportModal.tsx': 2,
  'src/components/admin/AdminLoginGate.tsx': 1,
};

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

function unlabelledControls(file: string, text: string): number {
  const sf = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let n = 0;
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
        if (!named && !textChildren && !insideFormField(element, sf)) n += 1;
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sf);
  return n;
}

test('no new form control lacks an accessible name', async () => {
  const root = new URL('../', import.meta.url).pathname;
  const over: string[] = [];
  const found: Record<string, number> = {};
  for (const f of await sources(root)) {
    const rel = f.replace(root, 'src/');
    const n = unlabelledControls(rel, await readFile(f, 'utf8'));
    if (n > 0) found[rel] = n;
    if (n > (BASELINE[rel] ?? 0)) over.push(`${rel}: ${n} unlabelled (baseline ${BASELINE[rel] ?? 0})`);
  }
  expect(over).toEqual([]);
  // Keep the baseline honest: a fixed file must lower its entry.
  expect(found).toEqual(BASELINE);
});
