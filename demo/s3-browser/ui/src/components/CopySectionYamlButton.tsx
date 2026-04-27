/**
 * CopySectionYamlButton — header-slot button that copies the
 * current Configuration page's section YAML to the clipboard.
 *
 * Replaces the earlier right-rail "Copy YAML" card which was
 * wasting a full column of horizontal space on every Configuration
 * page — a heavy cost for a single button, and it broke
 * responsive layout on viewports under ~1400px.
 *
 * Slots into `FullScreenHeader`'s `extra` row next to the existing
 * Export YAML / Import YAML buttons. Rendered only when the
 * current path has an associated section (Configuration pages);
 * hidden on Diagnostics, first-run setup, etc.
 */
import { useEffect, useRef, useState } from 'react';
import { Button, Dropdown, message } from 'antd';
import type { MenuProps } from 'antd';
import { CopyOutlined, DownOutlined } from '@ant-design/icons';
import type { SectionName } from '../adminApi';
import { getSectionYaml } from '../adminApi';
import { useColors } from '../ThemeContext';

interface Props {
  /** The active section for this page. Undefined = button doesn't render. */
  section?: SectionName;
}

export default function CopySectionYamlButton({ section }: Props) {
  const { TEXT_MUTED } = useColors();
  const [copying, setCopying] = useState(false);
  const mountedRef = useRef(true);
  useEffect(
    () => () => {
      mountedRef.current = false;
    },
    []
  );

  if (!section) return null;

  const handleCopy = async () => {
    setCopying(true);
    try {
      const yaml = await getSectionYaml(section);
      if (!mountedRef.current) return;
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(yaml);
        if (!mountedRef.current) return;
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

  const label = section.charAt(0).toUpperCase() + section.slice(1);
  const items: MenuProps['items'] = [
    {
      key: 'copy-section-yaml',
      icon: <CopyOutlined />,
      label: `Copy ${label} section YAML`,
      onClick: () => {
        void handleCopy();
      },
    },
  ];

  return (
    <Dropdown menu={{ items }} trigger={['click']} placement="bottomRight">
      <Button
        size="small"
        type="text"
        icon={<CopyOutlined />}
        loading={copying}
        title={`Section-scoped YAML tools for ${label}. This does not export the full config.`}
        style={{ color: TEXT_MUTED, fontFamily: 'var(--font-ui)' }}
      >
        <span className="hide-mobile" style={{ marginLeft: 4 }}>
          Section YAML
        </span>
        <DownOutlined className="hide-mobile" style={{ marginLeft: 4, fontSize: 10 }} />
      </Button>
    </Dropdown>
  );
}
