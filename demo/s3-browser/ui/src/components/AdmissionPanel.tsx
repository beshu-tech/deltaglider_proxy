/**
 * AdmissionPanel — top-level editor for the `admission` section.
 *
 * Wave 4 of the admin UI revamp plan, §7.1. Composes:
 *
 *   * `AdmissionBlockList`    — drag-reorderable list of operator
 *                                blocks; `@dnd-kit` reorder.
 *   * `AdmissionBlockEditorModal` — form for creating / editing a
 *                                single block.
 *   * `SynthesizedBlocksPreview`  — read-only list of the blocks
 *                                synthesised from
 *                                `storage.buckets[*].public_prefixes`.
 *   * `ApplyDialog`           — plan -> diff -> apply confirmation
 *                                before the section PUT.
 *
 * Flow:
 *
 *   1. Fetch `section/admission` (blocks[]) and `config` (for
 *      bucket_policies -> synthesised preview) in parallel.
 *   2. `useDirtySection('admission', blocks)` tracks unsaved edits.
 *      Add/edit/delete/reorder all mutate this local state only.
 *   3. Apply: call `validateSection` for the diff + warnings, show
 *      `ApplyDialog`, on confirm call `putSection`.
 *   4. Discard: revert the dirty state to the last-applied snapshot.
 */
import { useEffect, useRef, useState } from 'react';
import { Button, Alert, Typography, Space, Modal } from 'antd';
import {
  PlusOutlined,
  InfoCircleOutlined,
  ExclamationCircleOutlined,
} from '@ant-design/icons';
import type { AdmissionBlock, AdminConfig } from '../adminApi';
import { getAdminConfig } from '../adminApi';
import { useColors } from '../ThemeContext';
import { useSectionEditor } from '../useSectionEditor';
import AdmissionBlockList from './AdmissionBlockList';
import AdmissionBlockEditorModal from './AdmissionBlockEditorModal';
import SynthesizedBlocksPreview from './SynthesizedBlocksPreview';
import ApplyDialog from './ApplyDialog';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
  /** Navigate callback; used by the synthesised preview's
   *  "Edit in Storage" link to jump to the bucket editor. */
  onNavigateToBucket: (bucket: string) => void;
}

interface AdmissionSectionBody {
  blocks: AdmissionBlock[];
}

export default function AdmissionPanel({
  onSessionExpired,
  onNavigateToBucket,
}: Props) {
  const { BORDER, TEXT_MUTED } = useColors();

  const [config, setConfig] = useState<AdminConfig | null>(null);

  // The shared editor handles the admission section (blocks[]).
  // Local state is `AdmissionBlock[]`; wire shape is
  // `{ blocks: AdmissionBlock[] }` — converted via pick/toPayload.
  const {
    value: blocks,
    setValue: setBlocks,
    discard,
    isDirty,
    loading,
    applyOpen,
    applyResponse,
    applying,
    runApply,
    cancelApply,
    confirmApply,
  } = useSectionEditor<AdmissionSectionBody, AdmissionBlock[]>({
    section: 'admission',
    initial: [],
    onSessionExpired,
    pick: (body) => body?.blocks ?? [],
    toPayload: (v) => ({ blocks: v }),
    noun: 'admission blocks',
  });

  // We still need the bucket-policies for the synthesised-blocks
  // preview — fetch them alongside, independently of the section editor.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const cfg = await getAdminConfig();
        if (!cancelled) setConfig(cfg);
      } catch (e) {
        if (e instanceof Error && e.message.includes('401')) {
          onSessionExpired?.();
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [onSessionExpired]);

  // Modal state: when the operator clicks Add or Edit, we set
  // `editing` to the index (or -1 for Add) and `editingBlock` to
  // the block data (or null for Add).
  const [editingIndex, setEditingIndex] = useState<number | null>(null);
  const [editingBlock, setEditingBlock] = useState<AdmissionBlock | null>(null);

  const openAdd = () => {
    setEditingIndex(-1);
    setEditingBlock(null);
  };
  const openEdit = (i: number) => {
    setEditingIndex(i);
    setEditingBlock(blocks[i]);
  };
  const closeEditor = () => {
    setEditingIndex(null);
    setEditingBlock(null);
  };

  const handleSave = (updated: AdmissionBlock) => {
    if (editingIndex === null) return;
    if (editingIndex < 0) {
      // Add
      setBlocks([...blocks, updated]);
    } else {
      // Edit
      const next = blocks.slice();
      next[editingIndex] = updated;
      setBlocks(next);
    }
    closeEditor();
  };

  // Dedupe Modal.confirm: a rapid double-click on Delete must NOT
  // queue two stacked modals — both would call `setBlocks` in order,
  // dropping block `i` AND the block now at `i` (originally `i+1`).
  // We guard with a ref that tracks whether ANY delete confirm is
  // open; Modal.confirm's `afterClose` callback resets it.
  const deleteInFlightRef = useRef(false);
  const handleDelete = (i: number) => {
    if (deleteInFlightRef.current) return;
    deleteInFlightRef.current = true;
    const name = blocks[i].name;
    Modal.confirm({
      title: `Remove block "${name}"?`,
      icon: <ExclamationCircleOutlined />,
      content:
        'The block is removed from the local form state only — nothing persists until you click Apply.',
      okText: 'Remove',
      okButtonProps: { danger: true },
      cancelText: 'Cancel',
      onOk: () => {
        const next = blocks.slice();
        next.splice(i, 1);
        setBlocks(next);
      },
      afterClose: () => {
        deleteInFlightRef.current = false;
      },
    });
  };

  const otherNames = blocks
    .filter((_, i) => i !== editingIndex)
    .map((b) => b.name);

  return (
    <div
      style={{
        // Match the responsive-pad wrapper used by every other
        // Configuration page (CredentialsModePanel, advanced sub-
        // panels, etc.). Without this wrapper the admission list
        // renders flush against the sidebar's right edge and feels
        // visually disconnected from the section header above.
        // `maxWidth: 960` (wider than the 740 used by form panels)
        // so the admission block rows — which have drag handle +
        // block name + match summary + action badge + chevron —
        // have breathing room without forcing horizontal scroll on
        // long block names.
        maxWidth: 960,
        margin: '0 auto',
        padding: 'clamp(16px, 3vw, 24px)',
        display: 'flex',
        flexDirection: 'column',
        gap: 16,
      }}
    >
      {/* Dirty-state banner with Apply / Discard */}
      {isDirty && (
        <Alert
          type="warning"
          showIcon
          message="Unsaved changes to this section"
          description="Review the diff in the Apply dialog before persisting."
          action={
            <Space>
              <Button size="small" onClick={discard} disabled={applying}>
                Discard
              </Button>
              <Button
                type="primary"
                size="small"
                onClick={runApply}
                disabled={applying}
                loading={applying}
              >
                Apply
              </Button>
            </Space>
          }
        />
      )}

      {/* Operator-authored blocks */}
      <section>
        <header
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            paddingBottom: 8,
            marginBottom: 12,
            borderBottom: `1px solid ${BORDER}`,
          }}
        >
          <div>
            <h3 style={{ margin: 0, fontFamily: 'var(--font-ui)' }}>
              Operator-authored blocks
            </h3>
            <Text type="secondary" style={{ fontSize: 12 }}>
              Drag to reorder. First match wins. Operator blocks fire{' '}
              <b>before</b> synthesised public-prefix blocks below.
            </Text>
          </div>
          <Button icon={<PlusOutlined />} onClick={openAdd} disabled={loading}>
            Add block
          </Button>
        </header>
        <AdmissionBlockList
          blocks={blocks}
          onReorder={setBlocks}
          onEdit={openEdit}
          onDelete={handleDelete}
        />
      </section>

      {/* Synthesised blocks preview */}
      <section>
        <header
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            paddingBottom: 8,
            marginBottom: 12,
            borderBottom: `1px solid ${BORDER}`,
          }}
        >
          <div>
            <h3 style={{ margin: 0, fontFamily: 'var(--font-ui)' }}>
              Synthesised blocks (read-only)
            </h3>
            <Text type="secondary" style={{ fontSize: 12 }}>
              Derived from Storage → Buckets' public_prefixes. Edit there.
            </Text>
          </div>
          <Text type="secondary" style={{ fontSize: 11, color: TEXT_MUTED }}>
            <InfoCircleOutlined /> evaluated after operator blocks
          </Text>
        </header>
        {config && (
          <SynthesizedBlocksPreview
            bucketPolicies={config.bucket_policies}
            onEditInStorage={onNavigateToBucket}
          />
        )}
      </section>

      {/* Editor modal */}
      <AdmissionBlockEditorModal
        open={editingIndex !== null}
        initial={editingBlock}
        otherNames={otherNames}
        onCancel={closeEditor}
        onSave={handleSave}
      />

      {/* Apply confirmation dialog */}
      <ApplyDialog
        open={applyOpen}
        section="admission"
        response={applyResponse}
        onApply={confirmApply}
        onCancel={cancelApply}
        loading={applying}
      />
    </div>
  );
}
