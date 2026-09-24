import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/components/admin/logLevelPresets.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'logLevelPresets.ts',
});
const { logLevelRadio, findMatchingPreset } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

const INFO = 'deltaglider_proxy=info,tower_http=info';

// Preset matching ignores directive order and whitespace.
assert.equal(findMatchingPreset('tower_http=info, deltaglider_proxy=info'), INFO);
assert.equal(findMatchingPreset('info,deltaglider_proxy::api=trace'), null);

// A preset value shows its preset, a non-preset value shows Custom.
assert.deepEqual(logLevelRadio(INFO, false), { value: INFO, custom: false });
assert.deepEqual(logLevelRadio('info,x=trace', false), { value: '__custom__', custom: true });
// Clicking Custom on a preset value opens the editor without changing it.
assert.deepEqual(logLevelRadio(INFO, true), { value: '__custom__', custom: true });
// THE bug: after Discard the value is the preset again and the pick is
// cleared, so the radio must show the preset, not a stuck "Custom".
assert.deepEqual(logLevelRadio(INFO, false), { value: INFO, custom: false });
// The section API omits a default-valued log_level: absent means the
// server default (Debug), not "nothing selected".
assert.deepEqual(logLevelRadio(undefined, false), {
  value: 'deltaglider_proxy=debug,tower_http=debug',
  custom: false,
});

console.log('log-level radio regression checks passed');
