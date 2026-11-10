/**
 * THE row identity of the object browser (epic #146, F6).
 *
 * A table row is a folder or an object. The two share one text space (an
 * object may be named `folder:abc/`), so the kind is never read from the
 * text of a key. A row has a typed `RowId`; its `RowKey` is the opaque string
 * the table, the selection and the keyboard cursor hold. Only this module
 * makes a `RowKey` (`rowKeyOf`) and only it takes one apart (`parseRowKey`):
 * the encoding adds a one-letter tag, so no object key can equal a folder's
 * row key. A source guard (`src/__tests__/rowKeyGuard.test.ts`) and an ESLint
 * rule refuse any other code that builds or parses row-key text.
 *
 * Pure and React-free.
 */

/** What a row stands for. `prefix` ends with `/`; `key` is the S3 object key. */
export type RowId = { kind: 'folder'; prefix: string } | { kind: 'object'; key: string };

declare const rowKeyBrand: unique symbol;
/** The opaque, collision-free string form of a `RowId`. */
export type RowKey = string & { readonly [rowKeyBrand]: true };

const FOLDER_TAG = 'd:';
const OBJECT_TAG = 'o:';

export function rowKeyOf(id: RowId): RowKey {
  return ((id.kind === 'folder' ? FOLDER_TAG + id.prefix : OBJECT_TAG + id.key) as unknown) as RowKey;
}

export const folderRowKey = (prefix: string): RowKey => rowKeyOf({ kind: 'folder', prefix });
export const objectRowKey = (key: string): RowKey => rowKeyOf({ kind: 'object', key });

/** The inverse of `rowKeyOf`. Throws on text this module did not make. */
export function parseRowKey(rowKey: RowKey): RowId {
  const s: string = rowKey;
  if (s.startsWith(FOLDER_TAG)) return { kind: 'folder', prefix: s.slice(FOLDER_TAG.length) };
  if (s.startsWith(OBJECT_TAG)) return { kind: 'object', key: s.slice(OBJECT_TAG.length) };
  throw new Error(`Not a row key: ${JSON.stringify(s)}`);
}

/** A selection split by kind. A folder with an empty prefix (the bucket root) is dropped: no action expands the whole bucket. */
export function splitRowKeys(rowKeys: Iterable<RowKey>): { prefixes: string[]; keys: string[] } {
  const prefixes = new Set<string>();
  const keys: string[] = [];
  for (const rk of rowKeys) {
    const id = parseRowKey(rk);
    if (id.kind === 'folder') {
      if (id.prefix) prefixes.add(id.prefix);
    } else keys.push(id.key);
  }
  return { prefixes: [...prefixes], keys };
}
