/**
 * YAML Import/Export modal for the admin config.
 *
 * Two modes:
 *
 *   mode="export": fetches the canonical YAML from the server,
 *     displays it in a read-only textarea with a Copy-to-clipboard
 *     button. Secrets are redacted server-side (no SigV4 creds,
 *     no bootstrap hash, no AES key).
 *
 *   mode="import": operator pastes / types YAML into an editor.
 *     Three-step flow:
 *       1. Validate — POST /config/validate — shows warnings.
 *       2. Confirm — explicit "Apply and Persist" button.
 *       3. Apply — POST /config/apply — shows success + persisted
 *          path; operator refreshes the parent view to see the new
 *          state.
 *
 * The modal is deliberately a single component so both flows share
 * the same close/cancel affordances and the same syntax-highlighted
 * textarea. Split into two files only if the flows diverge further.
 */

import { useCallback, useEffect, useState } from 'react';
import { Alert, Button, Modal, Space, Typography, Tag, Input } from 'antd';
import { CopyOutlined, CheckOutlined, UploadOutlined, DownloadOutlined } from '@ant-design/icons';
import {
  exportConfigYaml,
  validateConfigYaml,
  applyConfigYaml,
  type ConfigApplyResponse,
} from '../adminApi';
import { useColors } from '../ThemeContext';

const { Text, Paragraph } = Typography;

interface YamlModalProps {
  open: boolean;
  mode: 'export' | 'import';
  onClose: () => void;
  /**
   * Called after a successful `applyConfigYaml`. Parent should refresh
   * whatever data it displays — the full config has been swapped.
   */
  onApplied?: () => void;
}

export function YamlImportExportModal({ open, mode, onClose, onApplied }: YamlModalProps) {
  const colors = useColors();
  const [yaml, setYaml] = useState('');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [copied, setCopied] = useState(false);
  const [validated, setValidated] = useState(false);
  const [applyResult, setApplyResult] = useState<ConfigApplyResponse | null>(null);

  // Reset state whenever the modal opens or the mode changes.
  useEffect(() => {
    if (!open) return;
    setError(null);
    setWarnings([]);
    setCopied(false);
    setValidated(false);
    setApplyResult(null);
    if (mode === 'export') {
      setLoading(true);
      exportConfigYaml()
        .then((text) => {
          setYaml(text);
          setLoading(false);
        })
        .catch((e) => {
          setError(e instanceof Error ? e.message : String(e));
          setLoading(false);
        });
    } else {
      setYaml('');
    }
  }, [open, mode]);

  const handleCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(yaml);
      setCopied(true);
      setTimeout(() => setCopied(false), 1800);
    } catch (e) {
      setError(`Clipboard write failed: ${e instanceof Error ? e.message : e}`);
    }
  }, [yaml]);

  const handleValidate = useCallback(async () => {
    setError(null);
    setWarnings([]);
    setValidated(false);
    if (!yaml.trim()) {
      setError('Paste or type some YAML first.');
      return;
    }
    setLoading(true);
    try {
      const resp = await validateConfigYaml(yaml);
      setWarnings(resp.warnings || []);
      if (resp.ok) {
        setValidated(true);
      } else if (resp.error) {
        setError(resp.error);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [yaml]);

  const handleApply = useCallback(async () => {
    setError(null);
    setLoading(true);
    try {
      const resp = await applyConfigYaml(yaml);
      setApplyResult(resp);
      if (!resp.applied) {
        setError(resp.error || 'Apply failed with no error message');
      } else if (onApplied) {
        onApplied();
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [yaml, onApplied]);

  const title =
    mode === 'export' ? (
      <>
        <DownloadOutlined style={{ marginRight: 8 }} />
        Export configuration as YAML
      </>
    ) : (
      <>
        <UploadOutlined style={{ marginRight: 8 }} />
        Import configuration from YAML
      </>
    );

  return (
    <Modal
      title={title}
      open={open}
      onCancel={onClose}
      width={820}
      maskClosable={false}
      footer={null}
      destroyOnClose
    >
      {mode === 'export' && (
        <Space direction="vertical" size="small" style={{ width: '100%' }}>
          <Paragraph type="secondary" style={{ marginBottom: 8 }}>
            This is the canonical four-section YAML of the current
            runtime config. Secrets (SigV4 credentials, bootstrap
            password hash, AES master key) are redacted — you must
            refill them from your secret manager on the target side.
            Drop the output into <Text code>deltaglider_proxy.yaml</Text>,
            point the server at it with{' '}
            <Text code>--config</Text>, and restart.
          </Paragraph>
          {error && <Alert type="error" message={error} showIcon />}
          <Input.TextArea
            value={yaml}
            readOnly
            rows={18}
            style={{
              fontFamily: 'ui-monospace, Menlo, monospace',
              fontSize: 12,
              background: colors.BG_ELEVATED,
            }}
            placeholder={loading ? 'Loading…' : ''}
          />
          <Space style={{ justifyContent: 'flex-end', width: '100%' }}>
            <Button onClick={onClose}>Close</Button>
            <Button
              type="primary"
              icon={copied ? <CheckOutlined /> : <CopyOutlined />}
              onClick={handleCopy}
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
            Paste a full YAML config document. Validate runs server-
            side (same logic as{' '}
            <Text code>deltaglider_proxy config lint</Text>); if the
            document is clean you can then apply it. Apply swaps the
            in-memory config atomically and persists to disk.{' '}
            <Text strong>Secrets</Text> (SigV4 credentials, bootstrap
            hash, AES key) are preserved from the running server if
            absent in your pasted YAML.
          </Paragraph>

          <Input.TextArea
            value={yaml}
            onChange={(e) => {
              setYaml(e.target.value);
              setValidated(false);
              setError(null);
              setWarnings([]);
              setApplyResult(null);
            }}
            rows={18}
            placeholder={'admission:\n  blocks: []\naccess:\n  iam_mode: gui\nstorage:\n  filesystem: /var/dgp\nadvanced:\n  cache_size_mb: 2048\n'}
            style={{
              fontFamily: 'ui-monospace, Menlo, monospace',
              fontSize: 12,
              background: colors.BG_ELEVATED,
            }}
            disabled={loading}
          />

          {error && (
            <Alert type="error" message="Validation error" description={error} showIcon />
          )}
          {warnings.length > 0 && (
            <Alert
              type="warning"
              message={`${warnings.length} warning${warnings.length === 1 ? '' : 's'}`}
              description={
                <ul style={{ margin: '4px 0', paddingLeft: 18 }}>
                  {warnings.map((w, i) => (
                    <li key={i} style={{ fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 12 }}>
                      {w}
                    </li>
                  ))}
                </ul>
              }
              showIcon
            />
          )}
          {validated && !applyResult && (
            <Alert
              type="success"
              message="YAML is valid"
              description="Click Apply to swap the running config."
              showIcon
            />
          )}
          {applyResult && applyResult.applied && (
            <Alert
              type="success"
              message="Config applied"
              description={
                <Space direction="vertical" size={2}>
                  <div>
                    <Tag color={applyResult.persisted ? 'green' : 'orange'}>
                      {applyResult.persisted ? 'persisted to disk' : 'in-memory only'}
                    </Tag>
                    {applyResult.requires_restart && (
                      <Tag color="orange">restart required for some fields</Tag>
                    )}
                  </div>
                  {applyResult.persisted_path && (
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      Written to: <Text code>{applyResult.persisted_path}</Text>
                    </Text>
                  )}
                </Space>
              }
              showIcon
            />
          )}

          <Space style={{ justifyContent: 'flex-end', width: '100%' }}>
            <Button onClick={onClose}>Close</Button>
            <Button onClick={handleValidate} loading={loading} disabled={!yaml.trim()}>
              Validate
            </Button>
            <Button
              type="primary"
              onClick={handleApply}
              loading={loading}
              disabled={!validated || !!applyResult?.applied}
            >
              Apply and Persist
            </Button>
          </Space>
        </Space>
      )}
    </Modal>
  );
}
