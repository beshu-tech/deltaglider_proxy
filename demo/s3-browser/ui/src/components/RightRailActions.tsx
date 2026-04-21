/**
 * RightRailActions — persistent right-rail on Configuration pages.
 *
 * Scope discipline: the rail is the voice of the **active section**.
 * Full-document YAML I/O lives in the shell header (Admin Settings
 * top bar); this rail never duplicates that. Two stacked groups:
 *
 *   1. Apply / Discard — drive the dirty-state flow for the active
 *      section. Only enabled when `dirty` is true.
 *   2. Copy YAML / Paste YAML — section-scoped round trip. Copy
 *      fetches `getSectionYaml(section)` and pushes it to the
 *      clipboard. Paste opens a parent-owned modal which validates
 *      the body against this section's schema before applying.
 *
 * ## Parent responsibilities
 *
 * This component is a dumb view over state the parent owns. It only
 * wires up button callbacks and decides which groups to render —
 * the actual Apply / Discard / Paste handlers live in the section
 * panel (which knows its form state).
 *
 * When the page doesn't have an associated section (Diagnostics
 * pages), omit `section` — the rail doesn't render at all. The
 * header Export / Import YAML buttons stay the only YAML surface
 * for diagnostic pages.
 */
import { useState } from 'react';
import { Button, message, Tooltip } from 'antd';
import {
  CheckCircleOutlined,
  UndoOutlined,
  CopyOutlined,
  ImportOutlined,
} from '@ant-design/icons';
import type { SectionName } from '../adminApi';
import { getSectionYaml } from '../adminApi';
import { useColors } from '../ThemeContext';

interface Props {
  /**
   * The active section (for the per-section Copy YAML button). When
   * omitted, the rail does not render — Diagnostics pages have no
   * section scope and use the header Export / Import YAML buttons
   * for any YAML I/O.
   */
  section?: SectionName;
  /** True when the active section has unsaved edits. */
  dirty?: boolean;
  /** Invoked on Apply. Parent runs validateSection -> dialog -> putSection. */
  onApply?: () => void;
  /** Invoked on Discard. Parent reverts form state to its snapshot. */
  onDiscard?: () => void;
  /**
   * Invoked on Paste YAML. Parent opens a section-scoped paste
   * dialog. Omitted when the section panel doesn't support paste
   * (typically because the section editor isn't dirty-tracked yet).
   */
  onPasteYaml?: () => void;
  /** True while Apply is in flight — disables both Apply and Discard. */
  applying?: boolean;
}

export default function RightRailActions({
  section,
  dirty,
  onApply,
  onDiscard,
  onPasteYaml,
  applying,
}: Props) {
  const { BG_CARD, BORDER, TEXT_MUTED } = useColors();
  const [copying, setCopying] = useState(false);

  // Rail only renders when the page belongs to a section. Diagnostics
  // pages (no section) show no rail — the header owns global YAML I/O.
  if (!section) return null;
  const hasDirtyActions = onApply && onDiscard;

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
    </aside>
  );
}
