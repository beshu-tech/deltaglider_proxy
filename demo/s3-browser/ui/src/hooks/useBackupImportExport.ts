/**
 * Full Backup export + restore for the admin System page. A picked zip opens
 * the restore-mode chooser (`restoreFile` set); a legacy JSON restores as
 * IAM-only straight away. A successful restore reloads the page so every
 * panel refetches.
 */
import { useCallback, useState } from 'react';
import { message } from 'antd';
import { exportBackup, importBackup, ImportBackupError, type ImportBackupMode } from '../adminApi';
import { isZipFile } from '../components/admin/backupFile';
import { normalizeUiError } from '../errorHandling';

export function useBackupImportExport() {
  const [restoreFile, setRestoreFile] = useState<File | null>(null);

  const exportFullBackup = useCallback(async () => {
    try {
      const { blob, filename } = await exportBackup();
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = filename;
      a.click();
      URL.revokeObjectURL(url);
      message.success('Full backup exported');
    } catch (e) {
      message.error('Export failed: ' + normalizeUiError(e, 'unknown'));
    }
  }, []);

  const runBackupImport = useCallback(async (file: File, mode: ImportBackupMode) => {
    try {
      const result = isZipFile(file)
        ? await importBackup(file, mode)
        : await importBackup(JSON.parse(await file.text()), 'iam-only');
      const ext = result.external_identities_created ?? 0;
      message.success(
        `Imported: ${result.users_created} users, ${result.groups_created} groups, ${ext} OIDC identities (${result.users_skipped} skipped)`
      );
      window.location.reload();
    } catch (e) {
      if (e instanceof ImportBackupError) {
        console.error('Full backup restore failed', {
          file: { name: file.name, type: file.type, size: file.size },
          status: e.status,
          response: e.response,
        });
        message.error(e.message, 8);
      } else {
        console.error('Full backup restore failed before request', e);
        message.error('Import failed: ' + normalizeUiError(e, 'invalid file'));
      }
    } finally {
      setRestoreFile(null);
    }
  }, []);

  const importFullBackup = useCallback(() => {
    const input = document.createElement('input');
    input.type = 'file';
    // Accept zip (new default) AND json (pre-v0.8.4 IAM-only backups
    // still round-trip via the content-type-sniffing import handler).
    input.accept = '.zip,.json,application/zip,application/json';
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      if (isZipFile(file)) {
        setRestoreFile(file);
      } else {
        runBackupImport(file, 'iam-only');
      }
    };
    input.click();
  }, [runBackupImport]);

  const cancelRestore = useCallback(() => setRestoreFile(null), []);

  return { restoreFile, cancelRestore, runBackupImport, exportFullBackup, importFullBackup };
}
