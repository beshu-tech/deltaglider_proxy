/**
 * src/safeStorage.ts: blocked site storage (the getter itself throws
 * SecurityError) and a full quota must degrade to "no stored value", never
 * throw into a render.
 */
import assert from 'node:assert/strict';
import { afterEach, test } from 'vitest';
import { readStorage, writeStorage, removeStorage } from '../safeStorage';

function memoryStorage(): Storage {
  const m = new Map<string, string>();
  return {
    getItem: (k: string) => (m.has(k) ? (m.get(k) as string) : null),
    setItem: (k: string, v: string) => {
      m.set(k, String(v));
    },
    removeItem: (k: string) => {
      m.delete(k);
    },
    clear: () => m.clear(),
    key: () => null,
    length: 0,
  };
}

afterEach(() => {
  Reflect.deleteProperty(globalThis, 'window');
});

test('working storage round-trips, per area', () => {
  Object.assign(globalThis, { window: { localStorage: memoryStorage(), sessionStorage: memoryStorage() } });
  writeStorage('k', 'v');
  assert.equal(readStorage('k'), 'v');
  assert.equal(readStorage('k', 'session'), null, 'areas are separate');
  writeStorage('k', 's', 'session');
  assert.equal(readStorage('k', 'session'), 's');
  removeStorage('k');
  assert.equal(readStorage('k'), null);
});

test('storage blocked: the property getter throws SecurityError', () => {
  const win = {};
  for (const name of ['localStorage', 'sessionStorage']) {
    Object.defineProperty(win, name, {
      get() {
        throw new Error('SecurityError: The operation is insecure.');
      },
    });
  }
  Object.assign(globalThis, { window: win });
  assert.equal(readStorage('k'), null);
  assert.doesNotThrow(() => writeStorage('k', 'v'));
  assert.doesNotThrow(() => removeStorage('k', 'session'));
});

test('quota exceeded on write', () => {
  Object.assign(globalThis, {
    window: {
      localStorage: {
        getItem: () => null,
        setItem: () => {
          throw new Error('QuotaExceededError');
        },
        removeItem: () => {},
      },
    },
  });
  assert.doesNotThrow(() => writeStorage('k', 'v'));
});
