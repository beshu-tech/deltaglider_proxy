/**
 * Section YAML modal + legacy button trigger for showing the current
 * Configuration page's section YAML.
 *
 * Replaces the earlier right-rail "Copy YAML" card which was
 * wasting a full column of horizontal space on every Configuration
 * page — a heavy cost for a single button, and it broke
 * responsive layout on viewports under ~1400px.
 *
 * The admin shell opens `SectionYamlModal` from the avatar menu's
 * Settings group.
 */
import { useEffect, useRef, useState } from 'react';
import { Alert, Button, Input, Modal, Space, message } from 'antd';
import { CopyOutlined } from '@ant-design/icons';
import type { SectionName } from '../adminApi';
import { getSectionYaml } from '../adminApi';
import { useColors } from '../ThemeContext';

interface SectionYamlModalProps {
  section?: SectionName;
  open: boolean;
  onClose: () => void;
}

export function SectionYamlModal({ section, open, onClose }: SectionYamlModalProps) {
  const colors = useColors();
  const [yaml, setYaml] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [copying, setCopying] = useState(false);
  const [copied, setCopied] = useState(false);
  const mountedRef = useRef(true);
  useEffect(
    () => () => {
      mountedRef.current = false;
    },
    []
  );

  useEffect(() => {
    if (!open || !section) return;

    let cancelled = false;
    setLoading(true);
    setError(null);
    setCopied(false);
    getSectionYaml(section)
      .then((text) => {
        if (cancelled || !mountedRef.current) return;
        setYaml(text);
      })
      .catch((e) => {
        if (cancelled || !mountedRef.current) return;
        setYaml('');
        setError(e instanceof Error ? e.message : 'unknown error');
      })
      .finally(() => {
        if (!cancelled && mountedRef.current) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, section]);

  if (!section) return null;

  const label = section.charAt(0).toUpperCase() + section.slice(1);

  const handleClose = () => {
    setCopied(false);
    onClose();
  };

  const handleCopy = async () => {
    if (!yaml) return;
    setCopying(true);
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(yaml);
        if (!mountedRef.current) return;
        setCopied(true);
        message.success(`Copied ${section} YAML to clipboard`);
      } else {
        // Clipboard API blocked / unavailable. Fall back to download.
        message.warning(
          'Clipboard API unavailable — falling back to a download. Check your browser permissions.'
        );
        const blob = new Blob([yaml], { type: 'application/yaml' });
        const url = URL.createObjectURL(blob);
        try {
          const a = document.createElement('a');
          a.href = url;
          a.download = `dgp-${section}.yaml`;
          a.click();
        } finally {
          URL.revokeObjectURL(url);
        }
      }
    } catch (e) {
      if (!mountedRef.current) return;
      message.error(
        `Copy failed: ${e instanceof Error ? e.message : 'unknown error'}`
      );
    } finally {
      if (mountedRef.current) setCopying(false);
    }
  };

  return (
    <Modal
      title={`${label} section YAML`}
      open={open}
      onCancel={handleClose}
      width={820}
      destroyOnClose
      footer={
        <Space style={{ justifyContent: 'flex-end', width: '100%' }}>
          <Button onClick={handleClose}>Close</Button>
          <Button
            type="primary"
            icon={<CopyOutlined />}
            loading={copying}
            onClick={() => {
              void handleCopy();
            }}
            disabled={!yaml || loading}
          >
            {copied ? 'Copied!' : 'Copy to clipboard'}
          </Button>
        </Space>
      }
    >
      <Space direction="vertical" size="small" style={{ width: '100%' }}>
        {error && <Alert type="error" message="Section YAML fetch failed" description={error} showIcon />}
        <Input.TextArea
          value={yaml}
          readOnly
          rows={18}
          placeholder={loading ? 'Loading...' : ''}
          style={{
            fontFamily: 'ui-monospace, Menlo, monospace',
            fontSize: 12,
            background: colors.BG_ELEVATED,
          }}
        />
      </Space>
    </Modal>
  );
}
