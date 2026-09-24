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
const { generateSetupYaml: emit } = await import(url);

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

console.log('setup-yaml regression tests passed');
