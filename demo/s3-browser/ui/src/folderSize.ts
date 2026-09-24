/**
 * Folder-size wording. A folder-size scan never sends a request per object,
 * so it can miss the original size of some objects (a delta or an encrypted
 * object that this proxy has not read or written since it started) and then
 * counts their smaller stored size. It can also stop at its object limit.
 * In both cases the total is a lower bound, and the UI must say so.
 */

interface ScanFlags {
  truncated?: boolean;
  sizes_estimated?: boolean;
}

/** True when the size shown for a folder is only a lower bound. */
export function folderSizeIsLowerBound(entry: ScanFlags, child?: { sizes_estimated?: boolean }): boolean {
  if (entry.truncated) return true;
  return child ? Boolean(child.sizes_estimated) : Boolean(entry.sizes_estimated);
}

/** Cell text: `≥ 12 MB` for a lower bound, else the size. */
export function folderSizeText(formattedSize: string, lowerBound: boolean): string {
  return lowerBound ? `≥ ${formattedSize}` : formattedSize;
}

/** Hover text for a computed folder size. */
export function folderSizeTitle(files: number, lowerBound: boolean): string {
  const count = `${files.toLocaleString()} ${files === 1 ? 'file' : 'files'}`;
  if (!lowerBound) return `${count}. Original size of the files.`;
  return (
    `${count}. At least this size: the original size of some files is not known yet, ` +
    'so they count their smaller stored size. Open or download a file to learn its size.'
  );
}
