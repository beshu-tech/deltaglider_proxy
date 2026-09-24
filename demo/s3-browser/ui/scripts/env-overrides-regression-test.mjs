import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/envOverrides.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'envOverrides.ts',
});
const { findEnvOverride, envOverrideText } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

const list = [
  { env: 'DGP_ACCESS_KEY_ID', yaml_path: 'access.access_key_id', secret: false, value: 'AKIAENV' },
  { env: 'DGP_SECRET_ACCESS_KEY', yaml_path: 'access.secret_access_key', secret: true },
  { env: 'DGP_REQUEST_TIMEOUT_SECS', secret: false, value: '60' },
];

// Matched by YAML path.
assert.equal(findEnvOverride(list, 'access.access_key_id')?.env, 'DGP_ACCESS_KEY_ID');
// Env-only settings are matched by variable name.
assert.equal(findEnvOverride(list, undefined, 'DGP_REQUEST_TIMEOUT_SECS')?.value, '60');
// No match, no list, no key.
assert.equal(findEnvOverride(list, 'advanced.cache_size_mb'), null);
assert.equal(findEnvOverride(undefined, 'access.access_key_id'), null);
assert.equal(findEnvOverride(list), null);
// An env-only entry has no yaml_path: an undefined yamlPath must not match it.
assert.equal(findEnvOverride(list, undefined, 'DGP_NOPE'), null);

// The effective value shows; a secret never does.
assert.equal(envOverrideText(list[0]), 'AKIAENV');
assert.equal(envOverrideText(list[1]), 'Set from the environment (value hidden)');
assert.equal(envOverrideText({ env: 'X', secret: true, value: 'leak' }), 'Set from the environment (value hidden)');

console.log('env-overrides regression checks passed');
