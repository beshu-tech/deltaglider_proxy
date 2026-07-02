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
import { useRef, useState } from 'react';
import { Button, Typography, Modal } from 'antd';
import {
  PlusOutlined,
  InfoCircleOutlined,
  ExclamationCircleOutlined,
} from '@ant-design/icons';
import type { AdmissionBlock } from '../adminApi';
import { useAdminConfig } from '../queries/config';
import { useColors } from '../ThemeContext';
import { useSectionEditor } from '../useSectionEditor';
import { contentColumn, CONTENT_FORM } from './shared-styles';
import AdmissionBlockList from './AdmissionBlockList';
import AdmissionBlockEditorModal from './AdmissionBlockEditorModal';
import SynthesizedBlocksPreview from './SynthesizedBlocksPreview';
import ApplyDialog from './ApplyDialog';
import StickyDirtyBar from './StickyDirtyBar';

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

  // Bucket-policies for the synthesised-blocks preview come from the
  // cached config query (shared, invalidated by config mutations).
  const { data: config } = useAdminConfig();

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

  // Modal state. The list passes us array indices, but indices go
  // stale the instant an earlier insert/delete shifts the array (e.g.
  // the operator opens Edit on row 2, then a queued mutation removes
  // row 0 — index 2 now points at a different block). `AdmissionBlock.name`
  // is unique (server-enforced) and is already the stable identity used
  // for @dnd-kit + React keys throughout AdmissionBlockList, so we key
  // editor state by name, not index. `ADD_SENTINEL` (rather than null)
  // distinguishes "Add modal open" from "no modal".
  const ADD_SENTINEL = '';
  const [editingName, setEditingName] = useState<string | null>(null);
  const [editingBlock, setEditingBlock] = useState<AdmissionBlock | null>(null);

  const openAdd = () => {
    setEditingName(ADD_SENTINEL);
    setEditingBlock(null);
  };
  const openEdit = (i: number) => {
    const block = blocks[i];
    if (!block) return;
    setEditingName(block.name);
    setEditingBlock(block);
  };
  const closeEditor = () => {
    setEditingName(null);
    setEditingBlock(null);
  };

  const handleSave = (updated: AdmissionBlock) => {
    if (editingName === null) return;
    if (editingName === ADD_SENTINEL) {
      // Add
      setBlocks([...blocks, updated]);
    } else {
      // Edit: replace the block whose name we opened. Resolving by name
      // (not a captured index) means an intervening reorder/delete can't
      // make us overwrite the wrong row. If the row vanished meanwhile,
      // map() is a no-op and the stale edit is silently dropped.
      setBlocks(blocks.map((b) => (b.name === editingName ? updated : b)));
    }
    closeEditor();
  };

  // Dedupe Modal.confirm: a rapid double-click on Delete must NOT
  // queue two stacked modals — both would call `setBlocks` in order
  // and, with index-based splices, drop the wrong block. Keying the
  // removal by name makes it idempotent (a second confirm filtering an
  // already-gone name is a no-op), but we still guard with a ref so the
  // operator never sees two stacked confirms.
  const deleteInFlightRef = useRef(false);
  const handleDelete = (i: number) => {
    if (deleteInFlightRef.current) return;
    const block = blocks[i];
    if (!block) return;
    deleteInFlightRef.current = true;
    const name = block.name;
    Modal.confirm({
      title: `Remove block "${name}"?`,
      icon: <ExclamationCircleOutlined />,
      content:
        'The block is removed from the local form state only — nothing persists until you click Apply.',
      okText: 'Remove',
      okButtonProps: { danger: true },
      cancelText: 'Cancel',
      onOk: () => {
        setBlocks(blocks.filter((b) => b.name !== name));
      },
      afterClose: () => {
        deleteInFlightRef.current = false;
      },
    });
  };

  const otherNames = blocks
    .filter((b) => b.name !== editingName)
    .map((b) => b.name);

  return (
    <div
      style={{
        ...contentColumn(CONTENT_FORM),
        display: 'flex',
        flexDirection: 'column',
        gap: 16,
      }}
    >
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
        open={editingName !== null}
        initial={editingBlock}
        otherNames={otherNames}
        onCancel={closeEditor}
        onSave={handleSave}
      />

      <StickyDirtyBar
        visible={isDirty}
        applying={applying}
        onDiscard={discard}
        onApply={runApply}
        floating
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
