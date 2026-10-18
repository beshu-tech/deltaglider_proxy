/**
 * Full Backup export + restore for the admin System page. A picked zip opens
 * the restore-mode chooser (`restoreFile` set); a legacy JSON restores as
 * IAM-only straight away. A successful restore reloads the page so every
 * panel refetches.
 */
import { createElement, useCallback, useState } from 'react';
import { Modal, message } from 'antd';
import {
  exportBackup,
  importBackup,
  ImportBackupError,
  type ImportBackupMode,
  type ImportBackupResult,
  type ImportIamMode,
} from '../adminApi';
import { isZipFile } from '../components/admin/backupFile';
import { normalizeUiError } from '../errorHandling';

/**
 * Pure: the paragraphs of the "Backup restored" dialog. Users that the
 * restore renamed (their name was taken) and policy rules it kept
 * contained (a condition the proxy cannot evaluate) are named, because the
 * operator must check them.
 */
export function restoreSummaryLines(result: ImportBackupResult): string[] {
  const ext = result.external_identities_created ?? 0;
  const lines = [
    `${result.users_created} users, ${result.groups_created} groups and ${ext} OIDC identities imported (${result.users_skipped} skipped); ${result.users_deleted ?? 0} users and ${result.groups_deleted ?? 0} groups that the backup does not hold were deleted.`,
  ];
  const renamed = result.users_renamed ?? [];
  if (renamed.length > 0) {
    lines.push(
      `${renamed.length} user${renamed.length === 1 ? ' was' : 's were'} imported under a new name, because another user already had the name: ${renamed.join(', ')}.`,
    );
  }
  const contained = result.contained_rules ?? [];
  if (contained.length > 0) {
    lines.push(
      `${contained.length} policy rule${contained.length === 1 ? ' has' : 's have'} a condition that the proxy cannot evaluate. The rule is kept, but an Allow grants nothing and a Deny applies without its condition: ${contained.join('; ')}.`,
    );
  }
  lines.push('The page reloads to show the restored state.');
  return lines;
}

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

  const runBackupImport = useCallback(
    async (file: File, mode: ImportBackupMode, iam: ImportIamMode = 'replace') => {
    try {
      const result = isZipFile(file)
        ? await importBackup(file, mode, iam)
        : await importBackup(JSON.parse(await file.text()), 'iam-only', iam);
      // The page reloads to pick up the restored state; a toast followed by
      // an immediate reload was never seen, so the outcome waits for an OK.
      Modal.success({
        title: 'Backup restored',
        content: createElement(
          'div',
          null,
          ...restoreSummaryLines(result).map((line, i) => createElement('p', { key: i, style: { margin: '0 0 8px' } }, line)),
        ),
        okText: 'Reload',
        onOk: () => window.location.reload(),
      });
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
    },
    []
  );

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
