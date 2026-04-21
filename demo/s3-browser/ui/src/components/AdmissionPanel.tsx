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
import { useCallback, useEffect, useRef, useState } from 'react';
import { Button, Alert, Typography, Space, Modal, message } from 'antd';
import {
  PlusOutlined,
  InfoCircleOutlined,
  ExclamationCircleOutlined,
} from '@ant-design/icons';
import type { AdmissionBlock, AdminConfig, SectionApplyResponse } from '../adminApi';
import {
  getAdminConfig,
  getSection,
  putSection,
  validateSection,
} from '../adminApi';
import { useColors } from '../ThemeContext';
import { useDirtySection, useApplyHandler } from '../useDirtySection';
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

  const [loading, setLoading] = useState(true);
  const [config, setConfig] = useState<AdminConfig | null>(null);
  const {
    value: blocks,
    isDirty,
    setValue: setBlocks,
    discard,
    markApplied,
    resetWith,
  } = useDirtySection<AdmissionBlock[]>('admission', []);

  // Modal state: when the operator clicks Add or Edit, we set
  // `editing` to the index (or -1 for Add) and `editingBlock` to
  // the block data (or null for Add).
  const [editingIndex, setEditingIndex] = useState<number | null>(null);
  const [editingBlock, setEditingBlock] = useState<AdmissionBlock | null>(null);

  // Apply-dialog state.
  //
  // `pendingApplyBlocks` captures the exact body that went to
  // `/validate` so `confirmApply` PUTs the same blocks the operator
  // saw in the diff, even if they kept interacting with the list
  // underneath the modal. Otherwise the diff shown and the body
  // persisted could diverge — adversarial review F5.
  const [applyOpen, setApplyOpen] = useState(false);
  const [applyResponse, setApplyResponse] = useState<SectionApplyResponse | null>(
    null
  );
  const [pendingApplyBlocks, setPendingApplyBlocks] = useState<
    AdmissionBlock[] | null
  >(null);
  const [applying, setApplying] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setLoading(true);
      const [sectionBody, cfg] = await Promise.all([
        getSection<AdmissionSectionBody>('admission'),
        getAdminConfig(),
      ]);
      const fetched = sectionBody?.blocks ?? [];
      resetWith(fetched);
      setConfig(cfg);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      message.error(
        `Failed to load admission blocks: ${e instanceof Error ? e.message : 'unknown'}`
      );
    } finally {
      setLoading(false);
    }
  }, [resetWith, onSessionExpired]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

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

  /**
   * Apply: call validate first to get the diff + warnings, then show
   * the ApplyDialog. On confirm, call putSection. On failure, leave
   * the form state as-is so the operator can fix and retry.
   */
  const runApply = async () => {
    // Snapshot the blocks at the moment the operator clicks Apply.
    // If they subsequently reorder/edit while the dialog is open,
    // the dialog's diff + PUT both use this snapshot — the
    // alternative (closure-read of `blocks` at confirm time) would
    // let the operator see diff A and persist body B.
    const snapshot = blocks.slice();
    try {
      const resp = await validateSection<AdmissionSectionBody>('admission', {
        blocks: snapshot,
      });
      setApplyResponse(resp);
      setPendingApplyBlocks(snapshot);
      setApplyOpen(true);
    } catch (e) {
      message.error(
        `Validate failed: ${e instanceof Error ? e.message : 'unknown'}`
      );
    }
  };

  const cancelApply = () => {
    setApplyOpen(false);
    setPendingApplyBlocks(null);
  };

  const confirmApply = async () => {
    if (!pendingApplyBlocks) return;
    setApplying(true);
    try {
      const resp = await putSection<AdmissionSectionBody>('admission', {
        blocks: pendingApplyBlocks,
      });
      if (!resp.ok) {
        // Server-side validation error (4xx). Surface the error but
        // keep the dialog open so the operator sees the reason next
        // to the diff they were about to confirm.
        message.error(resp.error || 'Apply failed');
        return;
      }
      message.success(
        resp.persisted_path
          ? `Applied + persisted to ${resp.persisted_path}`
          : 'Applied'
      );
      markApplied();
      setApplyOpen(false);
      setPendingApplyBlocks(null);
      // Re-fetch to pick up server-side normalisation.
      void refresh();
    } catch (e) {
      // Network / 5xx error. Close the dialog and force a refresh —
      // the server may have partially applied; the stale diff the
      // dialog was showing is no longer trustworthy (F11).
      message.error(
        `Apply failed: ${e instanceof Error ? e.message : 'unknown'}`
      );
      setApplyOpen(false);
      setPendingApplyBlocks(null);
      void refresh();
    } finally {
      setApplying(false);
    }
  };

  // ⌘S wiring: when dirty, ⌘S triggers the same flow as clicking
  // Apply — opens the validate → ApplyDialog sequence.
  useApplyHandler('admission', runApply, isDirty);

  const otherNames = blocks
    .filter((_, i) => i !== editingIndex)
    .map((b) => b.name);

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
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
