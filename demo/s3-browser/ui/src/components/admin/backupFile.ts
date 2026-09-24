/**
 * THE zip-vs-JSON test for a picked backup file. A zip is the Full Backup
 * (config + IAM + secrets) and offers the scoped restore modes; anything else
 * is a pre-v0.8.4 IAM-only JSON export. Browsers disagree on the MIME type of
 * a .zip (empty, `application/zip`, `application/x-zip-compressed`), so the
 * file name counts too.
 */
export function isZipFile(file: { name: string; type: string }): boolean {
  return (
    file.name.toLowerCase().endsWith('.zip') ||
    file.type === 'application/zip' ||
    file.type === 'application/x-zip-compressed'
  );
}
