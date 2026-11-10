/**
 * Folder-aware extraction of files from a drag-and-drop DataTransfer.
 *
 * `dataTransfer.files` lies about folders: dragging a directory from
 * Finder/Explorer yields a File-shaped entry (name = the folder, size = a few
 * hundred bytes of directory metadata) that CANNOT be read — uploading it
 * fails client-side with an opaque "TypeError: network error" before any
 * request is made. The only way to handle a dropped folder is the
 * `webkitGetAsEntry()` API: detect directory entries and walk them
 * recursively, collecting the real files inside.
 *
 * Files found inside a dropped folder are re-wrapped as
 * `new File([f], "<folder>/<sub>/<name>")` so their `name` carries the
 * folder-relative path — the upload queue's key builder
 * (`webkitRelativePath || file.name`) then recreates the folder structure
 * under the destination prefix, exactly like the "Select folder" input does
 * via `webkitRelativePath`. (That property is read-only on File, so the
 * path travels in `name` instead; the wrapper Blob references the original
 * file's bytes — nothing is copied eagerly.)
 *
 * CONTRACT: call this SYNCHRONOUSLY from the `drop` event handler. The
 * DataTransferItemList is only readable while the event is being dispatched,
 * so the entry/file extraction happens before the first `await`; only the
 * directory traversal itself is async (the entries stay valid).
 *
 * Duck-typed against minimal structural interfaces (not the DOM lib types)
 * so the traversal logic is unit-testable in Node with mock entries.
 */

/** Structural subset of FileSystemEntry / FileSystemFileEntry / FileSystemDirectoryEntry. */
export interface EntryLike {
  isFile: boolean;
  isDirectory: boolean;
  name: string;
  /** FileSystemFileEntry.file(successCb, errorCb) */
  file?: (onFile: (f: File) => void, onError: (e: unknown) => void) => void;
  /** FileSystemDirectoryEntry.createReader() */
  createReader?: () => ReaderLike;
}

export interface ReaderLike {
  /** Returns up to ~100 entries per call; an EMPTY batch signals the end. */
  readEntries: (onBatch: (entries: EntryLike[]) => void, onError: (e: unknown) => void) => void;
}

/** Structural subset of DataTransfer that this module needs. */
export interface DataTransferLike {
  items?: ArrayLike<{
    webkitGetAsEntry?: () => EntryLike | null;
    getAsFile?: () => File | null;
  }>;
  files: ArrayLike<File>;
}

/** Drain a directory reader: readEntries must be re-called until it returns []. */
function readAllEntries(reader: ReaderLike): Promise<EntryLike[]> {
  return new Promise((resolve, reject) => {
    const all: EntryLike[] = [];
    const step = () => {
      reader.readEntries((batch) => {
        if (batch.length === 0) {
          resolve(all);
        } else {
          all.push(...batch);
          step();
        }
      }, reject);
    };
    step();
  });
}

/** Carry the folder-relative path in the File's name (see module header). */
function withRelativePath(f: File, relPath: string): File {
  if (relPath === f.name) return f;
  return new File([f], relPath, { type: f.type, lastModified: f.lastModified });
}

/**
 * Recursively collect the files under one entry. `parentPath` is '' at the
 * top level and "<folder>/" inside directories. Unreadable files are skipped
 * rather than failing the whole drop — a partial upload the user can see
 * beats an all-or-nothing error. Each skip is recorded in `skipped` (F62):
 * the caller tells the user, the console is not the only place to learn it.
 */
async function collectEntry(entry: EntryLike, parentPath: string, skipped: string[]): Promise<File[]> {
  if (entry.isFile && entry.file) {
    try {
      const f = await new Promise<File>((resolve, reject) => entry.file!(resolve, reject));
      return [withRelativePath(f, parentPath + entry.name)];
    } catch (e) {
      console.warn(`Skipping unreadable dropped file "${parentPath}${entry.name}":`, e);
      skipped.push(parentPath + entry.name);
      return [];
    }
  }
  if (entry.isDirectory && entry.createReader) {
    try {
      const children = await readAllEntries(entry.createReader());
      const nested = await Promise.all(
        children.map((child) => collectEntry(child, `${parentPath}${entry.name}/`, skipped)),
      );
      return nested.flat();
    } catch (e) {
      console.warn(`Skipping unreadable dropped folder "${parentPath}${entry.name}":`, e);
      skipped.push(parentPath + entry.name + '/');
      return [];
    }
  }
  return [];
}

/**
 * Extract every real file from a drop, walking into dropped folders.
 *
 * Falls back to `dataTransfer.files` verbatim when the entry API is
 * unavailable or yields nothing (synthetic DataTransfers, older engines) —
 * matching the previous behaviour for plain file drops.
 *
 * `onSkipped` gets the paths of the files and folders that could not be read
 * (once, after the walk, in drop order); the files that could be read are
 * returned as usual.
 */
export function collectDroppedFiles(dt: DataTransferLike, onSkipped?: (paths: string[]) => void): Promise<File[]> {
  // ── Synchronous section: runs during the drop event dispatch. ──
  const plainFiles: File[] = Array.from(dt.files);
  const items = dt.items ? Array.from(dt.items) : [];
  const entries = items.map((it) => (it.webkitGetAsEntry ? it.webkitGetAsEntry() : null));

  if (entries.every((e) => e === null)) {
    // Entry API unavailable (or synthetic DataTransfer): no folders possible.
    return Promise.resolve(plainFiles);
  }

  // For null-entry items (rare mixed case), grab their plain File now —
  // getAsFile() also only works during the event.
  const looseFiles = items
    .map((it, i) => (entries[i] === null && it.getAsFile ? it.getAsFile() : null))
    .filter((f): f is File => f !== null);

  // ── Async section: entries stay valid after the event. ──
  return (async () => {
    // One list per top-level entry, joined in drop order (the walks run at the same time).
    const skippedPerEntry: string[][] = [];
    const nested = await Promise.all(
      entries
        .filter((e): e is EntryLike => e !== null)
        .map((entry) => {
          const skipped: string[] = [];
          skippedPerEntry.push(skipped);
          return collectEntry(entry, '', skipped);
        }),
    );
    const skipped = skippedPerEntry.flat();
    if (skipped.length > 0) onSkipped?.(skipped);
    return [...nested.flat(), ...looseFiles];
  })();
}

/** The words for the user after a drop that skipped `paths`. */
export function skippedDropMessage(paths: string[]): string {
  const shown = paths.slice(0, 3).join(', ');
  const more = paths.length > 3 ? ` and ${paths.length - 3} more` : '';
  const n = paths.length;
  return `${n} dropped ${n === 1 ? 'item' : 'items'} could not be read and ${n === 1 ? 'was' : 'were'} not added: ${shown}${more}.`;
}
