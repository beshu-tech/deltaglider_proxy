import type { ListResult, S3Object } from './types';
import { codePointCompare } from './utils';

/**
 * The folder after an auto-refresh that listed its first page only: that
 * page replaces every entry up to where it ended (`continuesAfter`), and
 * the entries the last full listing found after that point stay. A page
 * that holds the whole folder replaces everything.
 */
export function mergeFirstPage(
  previous: { objects: S3Object[]; folders: string[] },
  firstPage: ListResult,
): { objects: S3Object[]; folders: string[] } {
  const end = firstPage.continuesAfter;
  if (end === undefined) return { objects: firstPage.objects, folders: firstPage.folders };
  const after = (key: string) => codePointCompare(key, end) > 0;
  return {
    objects: [...firstPage.objects, ...previous.objects.filter((o) => after(o.key))],
    folders: [...firstPage.folders, ...previous.folders.filter(after)],
  };
}
