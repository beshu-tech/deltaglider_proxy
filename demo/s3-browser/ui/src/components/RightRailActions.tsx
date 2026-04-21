/**
 * RightRailActions — persistent right-rail on Configuration pages.
 *
 * Scope discipline (§3.3 of the revamp plan):
 *
 *   * **Section scope** (this component): `Copy YAML` for the
 *     active section. That is the rail's entire job.
 *   * **Document scope**: full-config Export / Import live in the
 *     shell header (Admin Settings top bar).
 *   * **Dirty-state Apply / Discard**: owned by each section panel
 *     as an inline banner. We rejected the "rail-driven Apply"
 *     design in Wave 4 because it required lifting every panel's
 *     form state up into `AdminPage` and threading it back down —
 *     the rail would have to know every panel's schema. Keeping
 *     Apply inside the panel that owns the form is the simpler
 *     architecture; `AdmissionPanel`'s dirty banner is the
 *     reference pattern future sections copy.
 *   * **Paste YAML (section-scoped import)**: designed but not
 *     built — the `YamlImportExportModal` is currently document-
 *     scoped. When it grows a `section` prop, the rail can carry
 *     a Paste YAML button; until then, nothing to advertise.
 *
 * When the page doesn't have an associated section (Diagnostics
 * pages), the rail doesn't render at all. The header Export /
 * Import YAML buttons stay the only YAML surface on those pages.
 */
import { useEffect, useRef, useState } from 'react';
import { Button, message, Tooltip } from 'antd';
import { CopyOutlined } from '@ant-design/icons';
import type { SectionName } from '../adminApi';
import { getSectionYaml } from '../adminApi';
import { useColors } from '../ThemeContext';

interface Props {
  /**
   * The active section, from [`sectionForPath`]. When omitted, the
   * rail does not render — Diagnostics pages have no section scope
   * and use the header Export / Import YAML buttons for any YAML
   * I/O.
   */
  section?: SectionName;
}

export default function RightRailActions({ section }: Props) {
  const { BG_CARD, BORDER, TEXT_MUTED } = useColors();
  const [copying, setCopying] = useState(false);
  // Track unmount so an in-flight Copy doesn't write stale YAML to
  // the clipboard after the rail has torn down (operator navigated
  // to a different section). AntD `loading` prevents a second
  // click during the fetch, but doesn't protect against the
  // mid-flight nav case.
  const mountedRef = useRef(true);
  useEffect(
    () => () => {
      mountedRef.current = false;
    },
    []
  );

  // Rail only renders when the page belongs to a section. Diagnostics
  // pages (no section) show no rail — the header owns global YAML I/O.
  if (!section) return null;

  const handleCopyYaml = async () => {
    setCopying(true);
    try {
      const yaml = await getSectionYaml(section);
      if (!mountedRef.current) return;
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(yaml);
        if (!mountedRef.current) return;
        message.success(`Copied ${section} YAML to clipboard`);
      } else {
        // Fallback: trigger a download. No clipboard permission needed.
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
    <aside
      aria-label="Section actions"
      style={{
        width: 180,
        flexShrink: 0,
        background: BG_CARD,
        border: `1px solid ${BORDER}`,
        borderRadius: 10,
        padding: 12,
        display: 'flex',
        flexDirection: 'column',
        gap: 6,
        position: 'sticky',
        top: 16,
        alignSelf: 'flex-start',
      }}
    >
      <div
        style={{
          fontSize: 10,
          fontWeight: 600,
          letterSpacing: 0.5,
          textTransform: 'uppercase',
          color: TEXT_MUTED,
          marginBottom: 4,
          fontFamily: 'var(--font-ui)',
        }}
      >
        {section} YAML
      </div>
      <Tooltip
        title={`Copy the ${section} section as canonical YAML. Secrets are redacted.`}
        placement="left"
      >
        <Button
          icon={<CopyOutlined />}
          loading={copying}
          onClick={handleCopyYaml}
          block
        >
          Copy YAML
        </Button>
      </Tooltip>
    </aside>
  );
}
