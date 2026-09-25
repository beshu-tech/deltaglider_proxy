/**
 * Pure destination-prefix normalization for DestinationPickerModal.
 *
 * The copy/move modal sends the user-entered destination path as an S3
 * `dest_prefix`. Historically it only stripped leading/trailing slashes, so a
 * fat-fingered `foo//bar` survived verbatim and produced keys with a literal
 * empty path segment (`foo//bar/...`). This collapses internal slash runs too,
 * matching the canonical key-segment cleanup already applied to upload paths in
 * useS3Browser.ts (`.replace(/\/{2,}/g, '/')`).
 *
 * Guarantees (see destPrefix regression test):
 *   - no leading slash, no trailing slash
 *   - no internal `//` run
 *   - an all-slash / empty input yields `''` (bucket root)
 */
export function normalizeDestPrefix(input: string): string {
  return input
    .replace(/^\/+/, '')
    .replace(/\/+$/, '')
    .replace(/\/{2,}/g, '/');
}

/**
 * The folder an item lands under when copied: a file keeps its basename, a
 * selected folder (`folder:<prefix>/`) keeps its own name, so both resolve
 * relative to their PARENT folder (see `expandSelection`).
 */
function parentFolder(selectionKey: string): string {
  if (selectionKey.startsWith('folder:')) {
    const pfx = selectionKey.slice('folder:'.length);
    return pfx.slice(0, pfx.slice(0, -1).lastIndexOf('/') + 1);
  }
  return selectionKey.slice(0, selectionKey.lastIndexOf('/') + 1);
}

/**
 * True when a copy/move to `destBucket`/`destPrefix` would write every
 * selected item back onto its own key: a move then does nothing and a copy
 * rewrites each object in place. `destPrefix` is the user's raw input.
 */
export function destinationIsSource(
  sourceBucket: string,
  selectionKeys: Iterable<string>,
  destBucket: string,
  destPrefix: string,
): boolean {
  if (sourceBucket !== destBucket) return false;
  const clean = normalizeDestPrefix(destPrefix);
  const dest = clean ? `${clean}/` : '';
  let any = false;
  for (const k of selectionKeys) {
    any = true;
    if (parentFolder(k) !== dest) return false;
  }
  return any;
}

/**
 * Why `path` (an object key or folder prefix) cannot be used, or null when it
 * can. Mirrors `check_object_path` in src/api/admin/path_guard.rs: the proxy
 * refuses a `.` or `..` segment and a NUL. Check before any request: the
 * browser's URL parser resolves `..`, so an S3 request is signed for one path
 * and sent to another, and the user sees only SignatureDoesNotMatch.
 */
export function keyPathError(path: string): string | null {
  if (path.includes('\0')) return 'The path contains a NUL character.';
  if (path.split('/').some((seg) => seg === '.' || seg === '..')) {
    return `The path "${path}" contains a "." or ".." folder. Folder names cannot be "." or "..".`;
  }
  return null;
}
