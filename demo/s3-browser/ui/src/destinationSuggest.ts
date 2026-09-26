/**
 * Pure helpers for the upload-destination autocomplete.
 *
 * The destination input holds a folder path like "tirso/sub". Suggestions come
 * from the bucket's REAL folders (S3 delimiter listing) one level at a time:
 * we list the folders under the path's parent and filter them by the segment
 * being typed. Kept React-free so a unit test can exercise the
 * splitting/filtering truth table (the async fetch lives in
 * useFolderSuggestions.ts).
 */

/**
 * Split the raw input into the PARENT prefix to list under and the TAIL
 * segment being typed. Leading slashes are noise ("/" placeholder habit);
 * everything up to and including the last "/" is the parent.
 *
 *   ""          → { parent: "",        tail: "" }      (list bucket root)
 *   "tir"       → { parent: "",        tail: "tir" }
 *   "tirso/"    → { parent: "tirso/",  tail: "" }      (list inside tirso/)
 *   "tirso/su"  → { parent: "tirso/",  tail: "su" }
 *   "/tirso"    → { parent: "",        tail: "tirso" }
 */
export function splitDestinationInput(raw: string): { parent: string; tail: string } {
  const clean = raw.replace(/^\/+/, '').replace(/\/{2,}/g, '/');
  const idx = clean.lastIndexOf('/');
  if (idx === -1) return { parent: '', tail: clean };
  return { parent: clean.slice(0, idx + 1), tail: clean.slice(idx + 1) };
}

/**
 * Filter the folders listed under `parent` down to those whose own segment
 * starts with the typed `tail` (case-insensitive). Folders arrive as full
 * prefixes ("tirso/sub/"); the comparison strips the parent first. Sorted for
 * a stable dropdown.
 */
export function filterFolderOptions(folders: string[], parent: string, tail: string): string[] {
  const needle = tail.toLowerCase();
  return folders
    .filter((f) => f.startsWith(parent))
    .filter((f) => f.slice(parent.length).toLowerCase().startsWith(needle))
    .sort();
}
