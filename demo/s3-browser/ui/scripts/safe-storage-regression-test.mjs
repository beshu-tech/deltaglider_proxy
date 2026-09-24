/**
 * Regression test for src/safeStorage.ts: blocked site storage (the
 * getter itself throws SecurityError) and a full quota must degrade to
 * "no stored value", never throw into a render.
 */
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

async function transpileAndImport(relPath) {
  const source = await readFile(new URL(relPath, import.meta.url), 'utf8');
  const out = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
    fileName: relPath,
  }).outputText;
  return import(`data:text/javascript;base64,${Buffer.from(out).toString('base64')}`);
}

const { readStorage, writeStorage, removeStorage } = await transpileAndImport('../src/safeStorage.ts');

function memoryStorage() {
  const m = new Map();
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => m.set(k, String(v)),
    removeItem: (k) => m.delete(k),
  };
}

// 1. Working storage round-trips, per area.
globalThis.window = { localStorage: memoryStorage(), sessionStorage: memoryStorage() };
writeStorage('k', 'v');
assert.equal(readStorage('k'), 'v');
assert.equal(readStorage('k', 'session'), null, 'areas are separate');
writeStorage('k', 's', 'session');
assert.equal(readStorage('k', 'session'), 's');
removeStorage('k');
assert.equal(readStorage('k'), null);

// 2. Storage blocked: the property getter throws SecurityError.
globalThis.window = {};
for (const name of ['localStorage', 'sessionStorage']) {
  Object.defineProperty(globalThis.window, name, {
    get() {
      throw new Error('SecurityError: The operation is insecure.');
    },
  });
}
assert.equal(readStorage('k'), null);
assert.doesNotThrow(() => writeStorage('k', 'v'));
assert.doesNotThrow(() => removeStorage('k', 'session'));

// 3. Quota exceeded on write.
globalThis.window = {
  localStorage: {
    getItem: () => null,
    setItem: () => {
      throw new Error('QuotaExceededError');
    },
    removeItem: () => {},
  },
};
assert.doesNotThrow(() => writeStorage('k', 'v'));

console.log('safe-storage-regression-test: OK');
