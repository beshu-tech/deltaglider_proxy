/**
 * The ONLY place that touches `localStorage` / `sessionStorage` directly
 * (enforced by the UI_RULES in eslint.config.mjs). Browsers with site storage
 * blocked throw `SecurityError` on property access, and a full quota
 * throws on write — a UI preference must never white-screen the app, so
 * every access degrades to "no stored value" instead.
 */
type Area = 'local' | 'session';

function area(which: Area): Storage | null {
  try {
    return which === 'local' ? window.localStorage : window.sessionStorage;
  } catch {
    return null;
  }
}

export function readStorage(key: string, which: Area = 'local'): string | null {
  try {
    return area(which)?.getItem(key) ?? null;
  } catch {
    return null;
  }
}

export function writeStorage(key: string, value: string, which: Area = 'local'): void {
  try {
    area(which)?.setItem(key, value);
  } catch {
    /* quota exceeded / storage blocked — best-effort */
  }
}

export function removeStorage(key: string, which: Area = 'local'): void {
  try {
    area(which)?.removeItem(key);
  } catch {
    /* storage blocked — nothing to remove */
  }
}
