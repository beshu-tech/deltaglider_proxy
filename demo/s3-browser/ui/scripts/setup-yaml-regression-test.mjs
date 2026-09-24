import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';
import { parse } from 'yaml';

// Transpile a TS module to an importable data: URL (no bundler).
async function loadModule(relPath, fileName) {
  const url = new URL(relPath, import.meta.url);
  const source = await readFile(url, 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ES2020,
      target: ts.ScriptTarget.ES2020,
    },
    fileName,
  });
  return `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
}

const url = await loadModule('../src/setupYaml.ts', 'setupYaml.ts');
const { generateSetupYaml: emit, setupEnvControl, setupStepComplete } = await import(url);
const NO_ENV_CONTROL = { adminAccessKeyId: false, adminSecretKey: false, backendKind: null };

// The wizard POSTs to /config/apply, which runs the server's `${env:NAME}`
// pre-pass over the raw text first (src/config/expansion.rs): `$$` → `$`,
// `${env:X}` → env value. Model that pass so the round-trip is end to end.
// A lone `$` before `{` would be expanded or rejected, so every `$` the
// emitter writes must be doubled.
function serverParse(text) {
  assert.ok(!/(^|[^$])(\$\$)*\$(?!\$)/.test(text), `undoubled $ in:\n${text}`);
  return parse(text.replace(/\$\$/g, '$'));
}
const generateSetupYaml = emit;

const base = {
  backendKind: 'filesystem',
  fsPath: './data',
  s3Endpoint: '',
  s3Region: 'us-east-1',
  s3ForcePathStyle: true,
  s3AccessKey: '',
  s3SecretKey: '',
  adminAccessKeyId: 'AKIAEXAMPLE',
  adminSecretKey: 'plain-secret',
  publicBucketName: '',
  enablePublicBucket: false,
};

// Every string the operator types must survive emit → parse byte-for-byte.
// A mangled secret silently locks the operator out (SigV4 mismatch).
const nasty = [
  'has"quote',
  'back\\slash',
  'new\nline',
  'tab\there',
  'unicodé ✓ 日本',
  'hash # not a comment',
  'colon: space',
  'C:\\data\\deltaglider',
  '${env:NOT_A_REF}',
  "single'quote",
  'a$$b',
  '$2b$10$bcryptish',
  'trailing$',
  ' leading and trailing ',
];

for (const v of nasty) {
  // access section
  const a = serverParse(generateSetupYaml({ ...base, adminAccessKeyId: v, adminSecretKey: v }));
  assert.equal(a.access.access_key_id, v, `access_key_id round-trip: ${JSON.stringify(v)}`);
  assert.equal(a.access.secret_access_key, v, `secret round-trip: ${JSON.stringify(v)}`);

  // filesystem path
  const f = serverParse(generateSetupYaml({ ...base, fsPath: v }));
  assert.equal(f.storage.filesystem, v, `fsPath round-trip: ${JSON.stringify(v)}`);

  // s3 backend fields
  const s = serverParse(
    generateSetupYaml({
      ...base,
      backendKind: 's3',
      s3Endpoint: v,
      s3Region: v,
      s3AccessKey: v,
      s3SecretKey: v,
    })
  );
  assert.equal(s.storage.s3, v);
  assert.equal(s.storage.region, v);
  assert.equal(s.storage.access_key_id, v);
  assert.equal(s.storage.secret_access_key, v);
}

// Unchanged shape for ordinary input.
const plain = serverParse(
  generateSetupYaml({
    ...base,
    backendKind: 's3',
    s3Endpoint: 'https://s3.example.com',
    s3Region: 'eu-central-1',
    s3AccessKey: 'K',
    s3SecretKey: 'S',
    s3ForcePathStyle: false,
    enablePublicBucket: true,
    publicBucketName: ' releases ',
  })
);
assert.deepEqual(plain, {
  access: { access_key_id: 'AKIAEXAMPLE', secret_access_key: 'plain-secret' },
  storage: {
    s3: 'https://s3.example.com',
    region: 'eu-central-1',
    access_key_id: 'K',
    secret_access_key: 'S',
    force_path_style: false,
    buckets: { releases: { public: true } },
  },
});

// Defaults omitted: us-east-1 region, path-style on, no creds → no access section.
const minimal = serverParse(generateSetupYaml({ ...base, adminAccessKeyId: '', adminSecretKey: '' }));
assert.deepEqual(minimal, { storage: { filesystem: './data' } });

// ── Env-controlled answers (#92): counted as given, left out of the YAML ──
const envList = [
  { env: 'DGP_ACCESS_KEY_ID', yaml_path: 'access.access_key_id', secret: false, value: 'AKIAENV', set: true },
  { env: 'DGP_SECRET_ACCESS_KEY', yaml_path: 'access.secret_access_key', secret: true, set: true },
  { env: 'DGP_S3_ENDPOINT', yaml_path: 'storage.backend.type', secret: false, value: 's3', set: true },
];
const env = setupEnvControl(envList);
assert.deepEqual(env, { adminAccessKeyId: true, adminSecretKey: true, backendKind: 's3' });
assert.deepEqual(setupEnvControl(undefined), NO_ENV_CONTROL);
assert.equal(
  setupEnvControl([{ env: 'DGP_DATA_DIR', yaml_path: 'storage.backend.type', secret: false, value: 'filesystem' }])
    .backendKind,
  'filesystem'
);

const isAbs = (p) => p.startsWith('/') || /^[A-Za-z]:[\\/]/.test(p);
const empty = { backendKind: 's3', fsPath: '', adminAccessKeyId: '', adminSecretKey: '', adminSecretKeyConfirm: '' };
// Without env: an S3 backend needs a passed test, the admin step needs keys.
assert.equal(setupStepComplete(1, empty, NO_ENV_CONTROL, false, isAbs), false);
assert.equal(setupStepComplete(1, empty, NO_ENV_CONTROL, true, isAbs), true);
assert.equal(setupStepComplete(2, empty, NO_ENV_CONTROL, false, isAbs), false);
// With env: the backend step and the admin step are already answered.
assert.equal(setupStepComplete(1, empty, env, false, isAbs), true);
assert.equal(setupStepComplete(2, empty, env, false, isAbs), true);
// A relative DGP_DATA_DIR still counts: the wizard does not own that value.
assert.equal(
  setupStepComplete(1, { ...empty, backendKind: 'filesystem', fsPath: '' }, { ...NO_ENV_CONTROL, backendKind: 'filesystem' }, false, isAbs),
  true
);
// Only the key id from env: the secret (and its confirmation) is still required.
const idOnly = { ...NO_ENV_CONTROL, adminAccessKeyId: true };
assert.equal(setupStepComplete(2, empty, idOnly, false, isAbs), false);
assert.equal(
  setupStepComplete(2, { ...empty, adminSecretKey: 'twelve-chars!', adminSecretKeyConfirm: 'twelve-chars!' }, idOnly, false, isAbs),
  true
);
// Windows paths are absolute too.
assert.equal(setupStepComplete(1, { ...empty, backendKind: 'filesystem', fsPath: 'C:\\dgp' }, NO_ENV_CONTROL, false, isAbs), true);

// The YAML leaves out everything the environment gives.
const envYaml = serverParse(generateSetupYaml({ ...base, backendKind: 's3', s3Endpoint: 'x' }, env));
assert.deepEqual(envYaml, {});
const envWithBucket = serverParse(
  generateSetupYaml({ ...base, enablePublicBucket: true, publicBucketName: 'releases' }, env)
);
assert.deepEqual(envWithBucket, { storage: { buckets: { releases: { public: true } } } });

console.log('setup-yaml regression tests passed');
