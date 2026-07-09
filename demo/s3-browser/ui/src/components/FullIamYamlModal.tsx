/**
 * Full-IAM YAML export / import modal.
 *
 * Distinct from {@link YamlImportExportModal}, which round-trips the
 * runtime *config* (admission / access / storage / advanced sections,
 * secrets redacted). THIS modal round-trips the entire *IAM* state —
 * users, groups, OAuth/auth providers, and group-mapping rules — as an
 * `access:`-shaped declarative YAML.
 *
 * Two modes:
 *
 *   mode="export": fetches the full IAM as YAML INCLUDING real secrets
 *     (secret_access_key, client_secret) so a re-import is lossless.
 *     ⚠️ The output holds LIVE credentials — the UI warns prominently
 *     and offers a copy + download-to-file affordance.
 *
 *   mode="import": operator pastes / types YAML. Flow:
 *       1. Validate — POST /config/declarative-iam-validate — a DRY RUN
 *          that returns a change summary (N created / M updated / K
 *          deleted) WITHOUT mutating any state.
 *       2. Confirm — the summary IS the confirmation; an explicit
 *          "Apply IAM changes" button reconciles atomically.
 *       3. Apply — POST /config/declarative-iam-apply.
 *
 * The reconcile is by-NAME and runs in a single SQLite transaction, so
 * a partial/failed import never leaves IAM half-written.
 */

import { useCallback, useEffect, useState } from 'react';
import { Alert, Button, Modal, Space, Typography, Tag, Input } from 'antd';
import {
  CopyOutlined,
  CheckOutlined,
  UploadOutlined,
  DownloadOutlined,
  WarningOutlined,
} from '@ant-design/icons';
import {
  exportFullIamYaml,
  validateFullIamYaml,
  applyFullIamYaml,
  type IamImportSummary,
} from '../adminApi';
import { useColors } from '../ThemeContext';
import { useCopyToClipboard } from '../useCopyToClipboard';
import { normalizeUiError } from '../errorHandling';

const { Text, Paragraph } = Typography;

interface Props {
  open: boolean;
  mode: 'export' | 'import';
  onClose: () => void;
  /** Called after a successful apply — parent should refresh IAM views. */
  onApplied?: () => void;
}

/** Total mutating changes across all categories (mapping rules excluded — they're a replace). */
function totalChanges(s: IamImportSummary): number {
  return (
    s.users_created +
    s.users_updated +
    s.users_deleted +
    s.groups_created +
    s.groups_updated +
    s.groups_deleted +
    s.providers_created +
    s.providers_updated +
    s.providers_deleted
  );
}

function SummaryTags({ s }: { s: IamImportSummary }) {
  const row = (label: string, created: number, updated: number, deleted: number) => {
    if (created === 0 && updated === 0 && deleted === 0) return null;
    return (
      <div style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap' }}>
        <Text strong style={{ minWidth: 78, display: 'inline-block' }}>{label}</Text>
        {created > 0 && <Tag color="green">{created} created</Tag>}
        {updated > 0 && <Tag color="blue">{updated} updated</Tag>}
        {deleted > 0 && <Tag color="red">{deleted} deleted</Tag>}
      </div>
    );
  };
  return (
    <Space direction="vertical" size={4} style={{ width: '100%' }}>
      {row('Users', s.users_created, s.users_updated, s.users_deleted)}
      {row('Groups', s.groups_created, s.groups_updated, s.groups_deleted)}
      {row('Providers', s.providers_created, s.providers_updated, s.providers_deleted)}
      {s.mapping_rules_replaced > 0 && (
        <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
          <Text strong style={{ minWidth: 78, display: 'inline-block' }}>Rules</Text>
          <Tag color="purple">{s.mapping_rules_replaced} mapping rule{s.mapping_rules_replaced === 1 ? '' : 's'} replaced</Tag>
        </div>
      )}
    </Space>
  );
}

export function FullIamYamlModal({ open, mode, onClose, onApplied }: Props) {
  const colors = useColors();
  const [yaml, setYaml] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { copy, copied } = useCopyToClipboard();
  const [preview, setPreview] = useState<IamImportSummary | null>(null);
  const [applied, setApplied] = useState<IamImportSummary | null>(null);

  // Reset whenever the modal opens or the mode flips.
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setError(null);
    setPreview(null);
    setApplied(null);
    if (mode === 'export') {
      setLoading(true);
      setYaml('');
      exportFullIamYaml(true)
        .then((text) => {
          if (cancelled) return;
          setYaml(text);
          setLoading(false);
        })
        .catch((e) => {
          if (cancelled) return;
          setError(normalizeUiError(e, "YAML operation failed"));
          setLoading(false);
        });
    } else {
      setYaml('');
    }
    return () => {
      cancelled = true;
    };
  }, [open, mode]);

  const handleCopy = useCallback(
    () => copy(yaml, { successMessage: 'Copied full IAM YAML to clipboard (contains live secrets)' }),
    [copy, yaml]
  );

  const handleDownload = useCallback(() => {
    const blob = new Blob([yaml], { type: 'application/yaml' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = 'deltaglider-iam.yaml';
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  }, [yaml]);

  const handleValidate = useCallback(async () => {
    setError(null);
    setPreview(null);
    setApplied(null);
    if (!yaml.trim()) {
      setError('Paste or type some IAM YAML first.');
      return;
    }
    setLoading(true);
    try {
      setPreview(await validateFullIamYaml(yaml));
    } catch (e) {
      setError(normalizeUiError(e, "YAML operation failed"));
    } finally {
      setLoading(false);
    }
  }, [yaml]);

  const handleApply = useCallback(async () => {
    setError(null);
    setLoading(true);
    try {
      const res = await applyFullIamYaml(yaml);
      setApplied(res);
      onApplied?.();
    } catch (e) {
      setError(normalizeUiError(e, "YAML operation failed"));
    } finally {
      setLoading(false);
    }
  }, [yaml, onApplied]);

  const title =
    mode === 'export' ? (
      <>
        <DownloadOutlined style={{ marginRight: 8 }} />
        Export full IAM as YAML
      </>
    ) : (
      <>
        <UploadOutlined style={{ marginRight: 8 }} />
        Import full IAM from YAML
      </>
    );

  return (
    <Modal title={title} open={open} onCancel={onClose} width={820} maskClosable={false} footer={null} destroyOnClose>
      {mode === 'export' && (
        <Space direction="vertical" size="small" style={{ width: '100%' }}>
          <Alert
            type="warning"
            showIcon
            icon={<WarningOutlined />}
            message="This file contains live credentials"
            description={
              <Paragraph style={{ marginBottom: 0 }}>
                The export includes real <Text code>secret_access_key</Text> and{' '}
                <Text code>client_secret</Text> values so a re-import is lossless.
                Treat the output like a password file — store it in a secret
                manager, never commit it to a public repo, and delete any local
                copy when done.
              </Paragraph>
            }
          />
          <Paragraph type="secondary" style={{ marginBottom: 8 }}>
            This is the entire IAM state — users, groups, OAuth providers, and
            group-mapping rules — as declarative <Text code>access:</Text> YAML.
            Re-import it on another instance via <Text strong>Import full IAM</Text>.
          </Paragraph>
          {error && <Alert type="error" message={error} showIcon />}
          <Input.TextArea
            value={yaml}
            readOnly
            rows={18}
            style={{ fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 12, background: colors.BG_ELEVATED }}
            placeholder={loading ? 'Loading…' : ''}
          />
          <Space style={{ justifyContent: 'flex-end', width: '100%' }}>
            <Button onClick={onClose}>Close</Button>
            <Button icon={<DownloadOutlined />} onClick={handleDownload} disabled={!yaml || loading}>
              Download .yaml
            </Button>
            <Button
              type="primary"
              icon={copied ? <CheckOutlined /> : <CopyOutlined />}
              onClick={() => {
                void handleCopy();
              }}
              disabled={!yaml || loading}
            >
              {copied ? 'Copied!' : 'Copy to clipboard'}
            </Button>
          </Space>
        </Space>
      )}

      {mode === 'import' && (
        <Space direction="vertical" size="small" style={{ width: '100%' }}>
          <Paragraph type="secondary" style={{ marginBottom: 8 }}>
            Paste a full IAM YAML document (the output of <Text strong>Export
            full IAM</Text>). Validate previews exactly what would change without
            touching anything. Apply makes everyone match this document, all at
            once — any users, groups, providers, or rules <Text strong>not</Text>{' '}
            in the YAML are <Text strong>deleted</Text>.
          </Paragraph>

          <Input.TextArea
            value={yaml}
            onChange={(e) => {
              setYaml(e.target.value);
              setPreview(null);
              setApplied(null);
              setError(null);
            }}
            rows={16}
            placeholder={'access:\n  iam_mode: declarative\n  iam_users:\n    - name: alice\n      access_key_id: AKIA…\n      secret_access_key: …\n  iam_groups: []\n  auth_providers: []\n  group_mapping_rules: []\n'}
            style={{ fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 12, background: colors.BG_ELEVATED }}
            disabled={loading}
          />

          {error && <Alert type="error" message="Import error" description={error} showIcon />}

          {preview && !applied && (
            <Alert
              type={preview.no_changes ? 'info' : 'warning'}
              showIcon
              message={
                preview.no_changes
                  ? 'No changes — the YAML matches the live IAM state'
                  : `Dry run: ${totalChanges(preview)} change${totalChanges(preview) === 1 ? '' : 's'} to apply`
              }
              description={preview.no_changes ? undefined : <SummaryTags s={preview} />}
            />
          )}

          {applied && (
            <Alert
              type="success"
              showIcon
              message={applied.no_changes ? 'Applied — nothing changed' : 'IAM import applied'}
              description={applied.no_changes ? undefined : <SummaryTags s={applied} />}
            />
          )}

          <Space style={{ justifyContent: 'flex-end', width: '100%' }}>
            <Button onClick={onClose}>Close</Button>
            <Button onClick={handleValidate} loading={loading} disabled={!yaml.trim()}>
              Validate (dry run)
            </Button>
            <Button
              type="primary"
              danger
              onClick={handleApply}
              loading={loading}
              disabled={!preview || preview.no_changes || !!applied}
            >
              Apply IAM changes
            </Button>
          </Space>
        </Space>
      )}
    </Modal>
  );
}
