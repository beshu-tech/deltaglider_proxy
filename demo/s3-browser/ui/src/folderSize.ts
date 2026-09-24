/**
 * Folder-size wording. A folder-size scan never sends a request per object,
 * so it can miss the original size of some objects (a delta or an encrypted
 * object that this proxy has not read or written since it started). Those
 * count their stored size instead: smaller for a delta, slightly larger for
 * an encrypted object, so the total is approximate. A scan can also stop at
 * its object limit, and then the total is a lower bound. The UI must say so.
 */

interface ScanFlags {
  truncated?: boolean;
  sizes_estimated?: boolean;
}

/** How exact a folder size is. */
export type FolderSizeBound = 'exact' | 'atLeast' | 'about';

/**
 * `about` wins over `atLeast`: an approximate total (which can be too large
 * on an encrypted backend) is not a proven lower bound, even when the scan
 * was also truncated.
 */
export function folderSizeBound(entry: ScanFlags, child?: { sizes_estimated?: boolean }): FolderSizeBound {
  const estimated = child ? Boolean(child.sizes_estimated) : Boolean(entry.sizes_estimated);
  if (estimated) return 'about';
  return entry.truncated ? 'atLeast' : 'exact';
}

/** Cell text: `≈ 12 MB`, `≥ 12 MB`, or the size. */
export function folderSizeText(formattedSize: string, bound: FolderSizeBound = 'exact'): string {
  if (bound === 'about') return `≈ ${formattedSize}`;
  if (bound === 'atLeast') return `≥ ${formattedSize}`;
  return formattedSize;
}

/** Hover text for a computed folder size. */
export function folderSizeTitle(files: number, bound: FolderSizeBound = 'exact'): string {
  const count = `${files.toLocaleString()} ${files === 1 ? 'file' : 'files'}`;
  if (bound === 'about') {
    return (
      `${count}. About this size: the original size of some files is not known yet, ` +
      'so they count their stored size, which can be smaller or larger. Open or download a file to learn its size.'
    );
  }
  if (bound === 'atLeast') {
    return `${count} counted. At least this size: the folder has more files than one scan counts.`;
  }
  return `${count}. Original size of the files.`;
}
