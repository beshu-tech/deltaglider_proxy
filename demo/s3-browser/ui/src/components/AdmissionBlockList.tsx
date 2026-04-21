/**
 * AdmissionBlockList — drag-reorderable list of operator-authored
 * blocks. Each row renders a summary of the match predicates + the
 * action badge, with Edit / Delete buttons.
 *
 * Wave 4 of the admin UI revamp plan, §7.1. Built on @dnd-kit
 * (see §4.4 of the plan). First-match-wins semantics make row order
 * load-bearing: the drag handle is the primary interaction on this
 * surface.
 *
 * ## Why block IDs are stable across reorders
 *
 * @dnd-kit needs each item to carry a stable ID that survives the
 * reorder. AdmissionBlock.name is unique (enforced by the server's
 * validator) so we use it directly — no synthetic `id` field. If
 * the operator renames a block, the list re-renders with the new
 * name as the new ID; that's a full rebuild, not a drag, so no
 * concurrency issue.
 */
import {
  DndContext,
  closestCenter,
  useSensor,
  useSensors,
  PointerSensor,
  KeyboardSensor,
  type DragEndEvent,
} from '@dnd-kit/core';
import {
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
  arrayMove,
} from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';
import { Button, Tag, Tooltip, Typography } from 'antd';
import {
  DragOutlined,
  EditOutlined,
  DeleteOutlined,
} from '@ant-design/icons';
import type { AdmissionBlock } from '../adminApi';
import { actionKind } from '../schemas/admissionSchema';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

interface Props {
  blocks: AdmissionBlock[];
  onReorder: (next: AdmissionBlock[]) => void;
  onEdit: (index: number) => void;
  onDelete: (index: number) => void;
}

export default function AdmissionBlockList({
  blocks,
  onReorder,
  onEdit,
  onDelete,
}: Props) {
  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: { distance: 4 },
    }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    })
  );

  const handleDragEnd = (evt: DragEndEvent) => {
    const { active, over } = evt;
    if (!over || active.id === over.id) return;
    const oldIndex = blocks.findIndex((b) => b.name === active.id);
    const newIndex = blocks.findIndex((b) => b.name === over.id);
    if (oldIndex < 0 || newIndex < 0) return;
    onReorder(arrayMove(blocks, oldIndex, newIndex));
  };

  if (blocks.length === 0) {
    return (
      <Text type="secondary" style={{ fontStyle: 'italic' }}>
        No operator-authored blocks yet. Click <b>Add block</b> to create the
        first one.
      </Text>
    );
  }

  return (
    <DndContext
      sensors={sensors}
      collisionDetection={closestCenter}
      onDragEnd={handleDragEnd}
    >
      <SortableContext
        items={blocks.map((b) => b.name)}
        strategy={verticalListSortingStrategy}
      >
        <div
          style={{
            display: 'flex',
            flexDirection: 'column',
            gap: 8,
          }}
        >
          {blocks.map((b, i) => (
            <SortableRow
              key={b.name}
              block={b}
              onEdit={() => onEdit(i)}
              onDelete={() => onDelete(i)}
            />
          ))}
        </div>
      </SortableContext>
    </DndContext>
  );
}

interface RowProps {
  block: AdmissionBlock;
  onEdit: () => void;
  onDelete: () => void;
}

function SortableRow({ block, onEdit, onDelete }: RowProps) {
  const { BORDER, BG_CARD, TEXT_MUTED } = useColors();
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: block.name });
  const style: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    border: `1px solid ${BORDER}`,
    background: BG_CARD,
    borderRadius: 8,
    padding: '8px 10px',
    display: 'grid',
    gridTemplateColumns: 'auto 1fr auto auto auto',
    gap: 12,
    alignItems: 'center',
    opacity: isDragging ? 0.4 : 1,
  };

  return (
    <div ref={setNodeRef} style={style} aria-label={`block ${block.name}`}>
      {/* Drag handle */}
      <Tooltip title="Drag to reorder" placement="left">
        <button
          {...attributes}
          {...listeners}
          aria-label="drag to reorder"
          style={{
            cursor: 'grab',
            background: 'transparent',
            border: 'none',
            padding: 4,
            color: TEXT_MUTED,
          }}
        >
          <DragOutlined />
        </button>
      </Tooltip>

      {/* Name + match summary */}
      <div style={{ minWidth: 0 }}>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <Text strong style={{ fontFamily: 'var(--font-mono)' }}>
            {block.name}
          </Text>
          <ActionBadge action={block.action} />
        </div>
        <Text
          type="secondary"
          style={{
            fontSize: 12,
            display: 'block',
            whiteSpace: 'nowrap',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
          }}
        >
          {matchSummary(block.match)}
        </Text>
      </div>

      {/* Spacer */}
      <span />

      {/* Edit */}
      <Tooltip title="Edit this block" placement="top">
        <Button
          size="small"
          type="text"
          icon={<EditOutlined />}
          onClick={onEdit}
          aria-label={`edit ${block.name}`}
        />
      </Tooltip>

      {/* Delete */}
      <Tooltip title="Remove this block" placement="top">
        <Button
          size="small"
          type="text"
          danger
          icon={<DeleteOutlined />}
          onClick={onDelete}
          aria-label={`delete ${block.name}`}
        />
      </Tooltip>
    </div>
  );
}

function ActionBadge({ action }: { action: AdmissionBlock['action'] }) {
  const kind = actionKind(action);
  const colour: Record<string, string> = {
    'allow-anonymous': 'green',
    deny: 'red',
    continue: 'blue',
    reject: 'orange',
  };
  const label =
    kind === 'reject' && typeof action !== 'string'
      ? `reject ${action.status}`
      : kind;
  return <Tag color={colour[kind]}>{label}</Tag>;
}

/**
 * Compact, one-line summary of a block's match predicates. Used in
 * the list row below the block name. Omits fields that are absent
 * (matches the server's behaviour of "absent = any").
 */
function matchSummary(match: AdmissionBlock['match']): string {
  const parts: string[] = [];
  if (match.method && match.method.length > 0)
    parts.push(`methods: ${match.method.join(',')}`);
  if (match.source_ip) parts.push(`source_ip: ${match.source_ip}`);
  if (match.source_ip_list && match.source_ip_list.length > 0)
    parts.push(
      `source_ip_list: ${match.source_ip_list.length} ${
        match.source_ip_list.length === 1 ? 'entry' : 'entries'
      }`
    );
  if (match.bucket) parts.push(`bucket: ${match.bucket}`);
  if (match.path_glob) parts.push(`path_glob: ${match.path_glob}`);
  if (match.authenticated !== undefined)
    parts.push(`auth: ${match.authenticated ? 'yes' : 'no'}`);
  if (match.config_flag) parts.push(`flag: ${match.config_flag}`);
  if (parts.length === 0) return 'matches every request';
  return parts.join(' · ');
}
