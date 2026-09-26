import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { test } from 'vitest';
import {
  findEnvOverride,
  envOverrideText,
  envOverrideSource,
  envOverrideHelp,
  backendEncryptionEnvPath,
  type EnvOverride,
} from '../envOverrides';

const list: EnvOverride[] = [
  { env: 'DGP_ACCESS_KEY_ID', yaml_path: 'access.access_key_id', secret: false, value: 'AKIAENV' },
  { env: 'DGP_SECRET_ACCESS_KEY', yaml_path: 'access.secret_access_key', secret: true },
  { env: 'DGP_REQUEST_TIMEOUT_SECS', secret: false, value: '60' },
];

test('findEnvOverride', () => {
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
});

test('envOverrideText: the effective value shows; a secret never does', () => {
  assert.equal(envOverrideText(list[0]), 'AKIAENV');
  assert.equal(envOverrideText(list[1]), 'Set from the environment (value hidden)');
  assert.equal(envOverrideText({ env: 'X', secret: true, value: 'leak' }), 'Set from the environment (value hidden)');
});

test('block members and the badge/help wording', () => {
  const unsetMember: EnvOverride = {
    env: 'DGP_S3_REGION',
    yaml_path: 'storage.backend.region',
    secret: false,
    value: 'us-east-1',
    set: false,
    activated_by: 'DGP_S3_ENDPOINT',
  };
  assert.equal(envOverrideText(unsetMember), 'us-east-1 (default)');
  assert.equal(envOverrideText({ ...unsetMember, value: undefined }), 'Not set');
  assert.equal(envOverrideText({ ...unsetMember, secret: true, value: undefined }), 'Not set');
  assert.equal(envOverrideSource(unsetMember), 'DGP_S3_ENDPOINT');
  assert.equal(envOverrideSource(list[0]), 'DGP_ACCESS_KEY_ID');
  assert.match(envOverrideHelp(unsetMember), /DGP_S3_ENDPOINT .*whole group.*set DGP_S3_REGION/);
  assert.match(envOverrideHelp(list[0]), /DGP_ACCESS_KEY_ID environment variable sets this value/);
  assert.equal(backendEncryptionEnvPath(null, 'key'), 'storage.backend_encryption.key');
  assert.equal(backendEncryptionEnvPath('eu-archive', 'kms_key_id'), 'storage.backends[eu-archive].encryption.kms_key_id');
});

test('every env-controlled YAML path the server reports has a UI consumer', async () => {
  // A path counts as consumed when some UI source names it as a string literal
  // (FormField yamlPath, useEnvOverride, findEnvOverride). The per-backend
  // encryption paths are built by backendEncryptionEnvPath (BackendsPanel).
  const rust = await readFile(new URL('../../../../../src/config/env_overrides.rs', import.meta.url), 'utf8');
  const rustBody = rust.slice(0, rust.indexOf('#[cfg(test)]'));
  const serverPaths = new Set(
    [...rustBody.matchAll(/"((?:advanced|access|storage)\.[a-z_.]+)"/g)].map((m) => m[1]),
  );
  assert.ok(serverPaths.size > 20, `parsed only ${serverPaths.size} paths from env_overrides.rs`);
  /** Paths with no GUI control at all: nothing to make read-only. */
  const NO_GUI_CONTROL = new Set([
    'advanced.max_passthrough_object_size',
    'advanced.config_sync_object_key',
    'storage.backend.force_path_style',
    'storage.backend.allow_local',
  ]);
  async function sources(dir: URL): Promise<string[]> {
    const out: string[] = [];
    for (const e of await readdir(dir, { withFileTypes: true })) {
      if (e.isDirectory() && e.name === '__tests__') continue;
      const p = new URL(`${e.name}${e.isDirectory() ? '/' : ''}`, dir);
      if (e.isDirectory()) out.push(...(await sources(p)));
      else if (/\.tsx?$/.test(e.name) && e.name !== 'envOverrides.ts') out.push(await readFile(p, 'utf8'));
    }
    return out;
  }
  const ui = (await sources(new URL('../', import.meta.url))).join('\n');
  for (const path of serverPaths) {
    if (path.startsWith('storage.backend_encryption.') || path.startsWith('storage.backends[')) continue;
    const consumed = ui.includes(`'${path}'`) || ui.includes(`"${path}"`);
    if (NO_GUI_CONTROL.has(path)) {
      assert.ok(!consumed, `${path} now has a UI consumer: remove it from NO_GUI_CONTROL`);
    } else {
      assert.ok(consumed, `${path} is env-controllable but no UI control checks it (useEnvOverride / FormField yamlPath)`);
    }
  }
  assert.ok(ui.includes('backendEncryptionEnvPath('), 'per-backend encryption env paths have no UI consumer');
});
