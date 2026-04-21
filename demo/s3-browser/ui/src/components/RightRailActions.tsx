/**
 * RightRailActions — Wave 3 foundation.
 *
 * The persistent right-rail visible on every configuration page
 * (§3.3 of the admin UI revamp plan). Three stacked button groups:
 *
 *   1. Apply / Discard — drive the dirty-state flow for the active
 *      section. Only enabled when `dirty` is true.
 *   2. Copy YAML / Paste YAML — section-scoped round trip. Copy
 *      fetches `getSectionYaml(section)` and pushes it to the
 *      clipboard; Paste opens an input modal (parent-owned) that
 *      calls `validateSection` then `putSection` on commit.
 *   3. Export all / Import all — full-document flows (parent opens
 *      the existing YamlImportExportModal).
 *
 * ## Parent responsibilities
 *
 * This component is a dumb view over state the parent owns. It only
 * wires up button callbacks and decides which groups to render — the
 * actual Apply / Discard / Paste handlers live in the section panel
 * (which knows its form state) and the Export / Import handlers on
 * AdminPage (which owns the modal).
 *
 * When the page doesn't have an associated section (Diagnostics
 * pages), omit `section`, `dirty`, `onApply`, and `onDiscard` — the
 * first group disappears and the rail becomes just the YAML controls.
 */
import { useState } from 'react';
import { Button, message, Tooltip, Space } from 'antd';
import {
  CheckCircleOutlined,
  UndoOutlined,
  CopyOutlined,
  FileTextOutlined,
  ImportOutlined,
  DownloadOutlined,
} from '@ant-design/icons';
import type { SectionName } from '../adminApi';
import { getSectionYaml } from '../adminApi';
import { useColors } from '../ThemeContext';

interface Props {
  /**
   * The active section (for the per-section Copy YAML button). When
   * omitted, the Copy/Paste group disappears entirely — useful on
   * Diagnostics pages that have no section scope.
   */
  section?: SectionName;
  /** True when the active section has unsaved edits. */
  dirty?: boolean;
  /** Invoked on Apply. Parent runs validateSection -> dialog -> putSection. */
  onApply?: () => void;
  /** Invoked on Discard. Parent reverts form state to its snapshot. */
  onDiscard?: () => void;
  /** Invoked on Paste YAML. Parent opens a section-scoped paste dialog. */
  onPasteYaml?: () => void;
  /** Invoked on Export all. Parent opens YamlImportExportModal in export mode. */
  onExportAll?: () => void;
  /** Invoked on Import all. Parent opens YamlImportExportModal in import mode. */
  onImportAll?: () => void;
  /** True while Apply is in flight — disables both Apply and Discard. */
  applying?: boolean;
}

export default function RightRailActions({
  section,
  dirty,
  onApply,
  onDiscard,
  onPasteYaml,
  onExportAll,
  onImportAll,
  applying,
}: Props) {
  const { BG_CARD, BORDER, TEXT_MUTED } = useColors();
  const [copying, setCopying] = useState(false);

  const hasSectionActions = section !== undefined;
  const hasDirtyActions = hasSectionActions && onApply && onDiscard;

  const handleCopyYaml = async () => {
    if (!section) return;
    setCopying(true);
    try {
      const yaml = await getSectionYaml(section);
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(yaml);
        message.success(`Copied ${section} YAML to clipboard`);
      } else {
        message.warning(
          'Clipboard API unavailable — falling back to a download. Check your browser permissions.'
        );
        // Fallback: trigger a download. No clipboard permission needed.
        const blob = new Blob([yaml], { type: 'application/yaml' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = `dgp-${section}.yaml`;
        a.click();
        URL.revokeObjectURL(url);
      }
    } catch (e) {
      message.error(
        `Copy failed: ${e instanceof Error ? e.message : 'unknown error'}`
      );
    } finally {
      setCopying(false);
    }
  };

  const groupStyle: React.CSSProperties = {
    display: 'flex',
    flexDirection: 'column',
    gap: 6,
  };
  const dividerStyle: React.CSSProperties = {
    height: 1,
    background: BORDER,
    margin: '8px 0',
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
        gap: 4,
        position: 'sticky',
        top: 16,
        alignSelf: 'flex-start',
      }}
    >
      {hasDirtyActions && (
        <>
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
            {section}
          </div>
          <div style={groupStyle}>
            <Tooltip
              title={
                dirty
                  ? 'Validate + apply this section. Shows a diff before committing.'
                  : 'No pending edits on this section.'
              }
              placement="left"
            >
              <Button
                type="primary"
                icon={<CheckCircleOutlined />}
                disabled={!dirty || applying}
                loading={applying}
                onClick={onApply}
                block
              >
                Apply
              </Button>
            </Tooltip>
            <Tooltip
              title={
                dirty
                  ? 'Revert this section to the server state.'
                  : 'No pending edits to discard.'
              }
              placement="left"
            >
              <Button
                icon={<UndoOutlined />}
                disabled={!dirty || applying}
                onClick={onDiscard}
                block
              >
                Discard
              </Button>
            </Tooltip>
          </div>
          <div style={dividerStyle} />
        </>
      )}

      {hasSectionActions && (
        <>
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
          <div style={groupStyle}>
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
            {onPasteYaml && (
              <Tooltip
                title="Paste a YAML fragment and apply it to this section. Validate runs first; no state change on failure."
                placement="left"
              >
                <Button icon={<ImportOutlined />} onClick={onPasteYaml} block>
                  Paste YAML
                </Button>
              </Tooltip>
            )}
          </div>
          <div style={dividerStyle} />
        </>
      )}

      {(onExportAll || onImportAll) && (
        <>
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
            Full config
          </div>
          <Space direction="vertical" size={6} style={{ width: '100%' }}>
            {onExportAll && (
              <Tooltip
                title="Copy the entire runtime config as canonical YAML."
                placement="left"
              >
                <Button
                  icon={<DownloadOutlined />}
                  onClick={onExportAll}
                  block
                >
                  Export all
                </Button>
              </Tooltip>
            )}
            {onImportAll && (
              <Tooltip
                title="Paste a full YAML document — validate, then apply + persist."
                placement="left"
              >
                <Button
                  icon={<FileTextOutlined />}
                  onClick={onImportAll}
                  block
                >
                  Import all
                </Button>
              </Tooltip>
            )}
          </Space>
        </>
      )}
    </aside>
  );
}
