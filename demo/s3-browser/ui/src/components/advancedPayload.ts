/**
 * Pure merge-patch payload helper for the Advanced sub-panels.
 *
 * Extracted to its own module (no React / antd imports) so the Node
 * unit test can import it directly.
 *
 * ## Why this exists
 *
 * Each Advanced sub-panel PUTs only the subset of `advanced.*` fields
 * it owns, and the server applies RFC 7396 merge-patch: a key set to
 * JSON `null` DELETES the field, an absent key is a NO-OP.
 *
 * The form represents a "cleared" scalar as `undefined`. `JSON.stringify`
 * DROPS `undefined`-valued keys, so a cleared field arrived at the server
 * as an absent key → no-op → the field could never be cleared.
 *
 * `undefinedToNullSubset` maps each owned key's `undefined` to explicit
 * `null` (merge-patch delete) and copies every other value through
 * unchanged. Fields that were never set are already absent server-side,
 * so `null` = delete = no-op there too; the only behavioral change is
 * that a user-cleared field now actually clears.
 *
 * It does NOT recurse into nested objects (e.g. `tls`): a populated
 * `tls` object passes through as-is (its own `cert_path` / `key_path`
 * already emit explicit `null` where needed), and a `tls: null` on an
 * already-absent key is a harmless no-op delete.
 */
export function undefinedToNullSubset<T extends object>(
  value: T,
  keys: ReadonlyArray<keyof T>
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const k of keys) {
    const v = value[k];
    out[k as string] = v === undefined ? null : v;
  }
  return out;
}
