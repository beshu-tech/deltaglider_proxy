// Must-fail fixtures for the UI lint guards (eslint.config.mjs): each snippet
// below is code a guard claims to refuse, run through the REAL config. A
// guard that a trivial rewrite slips past (`antd/es/tooltip`,
// `globalThis.localStorage`, `color: 'white'`, `Object.assign(el.style)`) has
// a fixture here, and so has the plain form it already caught.
import assert from 'node:assert/strict';
import { ESLint } from 'eslint';
import { beforeAll, describe, test } from 'vitest';

let eslint: ESLint;
beforeAll(() => {
  eslint = new ESLint({ overrideConfigFile: 'eslint.config.mjs', cwd: process.cwd() });
});

async function messages(code: string, filePath = 'src/components/Fixture.tsx'): Promise<string[]> {
  const [res] = await eslint.lintText(code, { filePath });
  return res.messages
    .filter((m) => m.ruleId?.startsWith('no-restricted'))
    .map((m) => `${m.ruleId}: ${m.message}`);
}

/** [name, code, substring that the refusal message must contain] */
const BAD: [string, string, string][] = [
  // AntD tooltip / popover / dropdown / autocomplete: named and deep imports.
  ['import { Tooltip } from antd', `import { Tooltip } from 'antd';\nexport const A = () => <Tooltip title="x" />;`, 'tooltip'],
  ['import { Popover } from antd', `import { Popover } from 'antd';\nexport const A = () => <Popover />;`, 'tooltip'],
  ['deep import antd/es/tooltip', `import Tooltip from 'antd/es/tooltip';\nexport const A = () => <Tooltip />;`, 'tooltip'],
  ['deep import antd/lib/tooltip', `import Tooltip from 'antd/lib/tooltip';\nexport const A = () => <Tooltip />;`, 'tooltip'],
  ['deep import antd/es/popover', `import Popover from 'antd/es/popover';\nexport const A = () => <Popover />;`, 'tooltip'],
  ['deep import antd/es/dropdown', `import Dropdown from 'antd/es/dropdown';\nexport const A = () => <Dropdown />;`, 'KeyboardDropdown'],
  ['deep import antd/es/auto-complete', `import AutoComplete from 'antd/es/auto-complete';\nexport const A = () => <AutoComplete />;`, 'SimpleAutoComplete'],
  ['namespace import of antd', `import * as antd from 'antd';\nexport const A = () => <antd.Tooltip />;`, 'namespace import'],
  ['ellipsis.tooltip', `import { Typography } from 'antd';\nexport const A = () => <Typography.Text ellipsis={{ tooltip: 'x' }}>t</Typography.Text>;`, 'ellipsis.tooltip'],

  // Web Storage and confirm: bare, window., globalThis., self., computed.
  ['bare localStorage', `export const a = () => localStorage.getItem('k');`, 'safeStorage'],
  ['window.localStorage', `export const a = () => window.localStorage.getItem('k');`, 'safeStorage'],
  ['globalThis.localStorage', `export const a = () => globalThis.localStorage.getItem('k');`, 'safeStorage'],
  ['self.localStorage', `export const a = () => self.localStorage.getItem('k');`, 'safeStorage'],
  ['window["localStorage"]', `export const a = () => window['localStorage'].getItem('k');`, 'safeStorage'],
  ['globalThis.sessionStorage', `export const a = () => globalThis.sessionStorage.getItem('k');`, 'safeStorage'],
  ['bare confirm', `export const a = () => confirm('sure?');`, 'confirmDialog'],
  ['globalThis.confirm', `export const a = () => globalThis.confirm('sure?');`, 'confirmDialog'],

  // Clipboard.
  ['navigator.clipboard', `export const a = () => navigator.clipboard.writeText('x');`, 'useCopyToClipboard'],
  ['window.navigator.clipboard', `export const a = () => window.navigator.clipboard.writeText('x');`, 'useCopyToClipboard'],
  ['globalThis.navigator.clipboard', `export const a = () => globalThis.navigator.clipboard.writeText('x');`, 'useCopyToClipboard'],

  // Colour literals.
  ['hex literal', `export const s = { color: '#ff0000' };`, 'No colour literal'],
  ['rgb() literal', `export const s = { color: 'rgb(1,2,3)' };`, 'No colour literal'],
  ['named colour in style color', `export const A = () => <div style={{ color: 'white' }} />;`, 'No named colour'],
  ['named colour in style background', `export const A = () => <div style={{ background: 'red' }} />;`, 'No named colour'],
  ['named colour in border shorthand', `export const A = () => <div style={{ border: '1px solid white' }} />;`, 'No named colour'],
  ['named colour in outline shorthand', `export const A = () => <div style={{ outline: '2px dashed gold' }} />;`, 'No named colour'],
  ['named colour in boxShadow', `export const s = { boxShadow: 'inset 0 0 4px black' };`, 'No named colour'],
  ['named colour in background gradient', `export const s = { background: 'linear-gradient(red, blue)' };`, 'No named colour'],
  ['named colour in SVG fill', `export const A = () => <svg><path fill="black" /></svg>;`, 'No named colour'],
  ['hex-alpha concatenation', 'export const f = (c: string) => `${c}22`;', 'hex-alpha'],

  // Hover styling in JS.
  ['onMouseEnter style assignment', `export const A = () => <div onMouseEnter={(e) => { e.currentTarget.style.background = 'x'; }} />;`, 'dg-hover'],
  ['onMouseEnter Object.assign(style)', `export const A = () => <div onMouseEnter={(e) => { Object.assign(e.currentTarget.style, { opacity: 1 }); }} />;`, 'dg-hover'],
  ['onMouseLeave style.setProperty', `export const A = () => <div onMouseLeave={(e) => { e.currentTarget.style.setProperty('opacity', '1'); }} />;`, 'dg-hover'],
];

const GOOD: [string, string][] = [
  ['AntD Tag preset colour', `export const s = { label: 'x', color: 'red' };`],
  ['transparent / inherit / currentColor', `export const A = () => <div style={{ color: 'inherit', background: 'transparent', borderColor: 'currentColor' }} />;`],
  ['named import of Button', `import { Button } from 'antd';\nexport const A = () => <Button />;`],
  ['a theme var', `export const A = () => <div style={{ color: 'var(--dg-text-primary)' }} />;`],
  // UI text that holds a style keyword and a colour word is not a style value.
  ['help text: "Consolidated ... green"', `export const t = 'Consolidated view: a green tick means the run is done';`],
  ['help text: "Dashed ... red"', `export const t = 'Dashed lines show the target. The red line shows the limit.';`],
  ['JSX text: "Inset the tan box"', `export const A = () => <p>{'Inset the tan box'}</p>;`],
  ['style border without a named colour', `export const A = () => <div style={{ border: '1px solid var(--dg-border)' }} />;`],
  ['hover in CSS class', `export const A = () => <div className="dg-hover-surface" />;`],
];

describe('UI lint guards refuse what they claim to refuse', () => {
  for (const [name, code, needle] of BAD) {
    test(`refuses: ${name}`, async () => {
      const msgs = await messages(code);
      assert.ok(msgs.length > 0, `the config accepted: ${code}`);
      assert.ok(msgs.some((m) => m.includes(needle)), `refused for another reason (want "${needle}"): ${msgs.join(' | ')}`);
    });
  }
  for (const [name, code] of GOOD) {
    test(`accepts: ${name}`, async () => {
      assert.deepEqual(await messages(code), []);
    });
  }
});
