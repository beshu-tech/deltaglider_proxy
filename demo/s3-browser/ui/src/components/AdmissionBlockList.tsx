/**
 * AdmissionBlockList — drag-reorderable list of operator-authored
 * blocks. Each row renders a summary of the match predicates + the
 * action badge, an Edit button, and a "⋯" menu that holds Remove.
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
import { Button, Tag, Typography } from 'antd';
import {
  DragOutlined,
  EditOutlined,
  DeleteOutlined,
} from '@ant-design/icons';
import type { AdmissionBlock } from '../adminApi';
import { actionKind } from '../schemas/admissionSchema';
import { useColors } from '../ThemeContext';
import RowActionsMenu from './RowActionsMenu';

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

  // The empty list is AdmissionPanel's EmptyState.
  if (blocks.length === 0) return null;

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
    padding: '8px 12px',
    // Flex with wrap, not a fixed grid: the text block keeps at least
    // RULE_TEXT_MIN_WIDTH, so on a phone the buttons move under it instead
    // of squeezing the rule name into a 60 px column.
    display: 'flex',
    flexWrap: 'wrap',
    columnGap: 12,
    rowGap: 4,
    alignItems: 'center',
    opacity: isDragging ? 0.4 : 1,
  };

  return (
    <div ref={setNodeRef} style={style} aria-label={`rule ${block.name}`}>
      {/* Drag handle */}
      <button
        {...attributes}
        {...listeners}
        title="Drag to reorder"
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

      {/* Name + match summary */}
      <RuleRowText name={block.name} action={block.action}>
        {matchSummary(block.match)}
      </RuleRowText>

      {/* Edit */}
      <Button
        size="small"
        type="text"
        icon={<EditOutlined />}
        onClick={onEdit}
        title="Edit this rule"
        aria-label={`edit ${block.name}`}
      />

      {/* Remove: behind "⋯", confirmed by the menu. */}
      <RowActionsMenu
        label={`More actions for rule ${block.name}`}
        actions={[
          {
            key: 'remove',
            label: 'Remove rule…',
            icon: <DeleteOutlined />,
            danger: true,
            confirm: {
              title: `Remove rule "${block.name}"?`,
              content: 'The rule is removed from this page only. Nothing changes until you review and apply.',
              okText: 'Remove',
            },
            onSelect: onDelete,
          },
        ]}
      />
    </div>
  );
}

/** The narrowest the text block of a rule row gets before its buttons wrap. */
const RULE_TEXT_MIN_WIDTH = 200;

/**
 * The text block of a rule row: the name and the action tag on one line
 * (the tag wraps under a long name), the summary under them in full —
 * the facts the row exists to show, so they wrap instead of being cut.
 * It grows to fill the row and keeps RULE_TEXT_MIN_WIDTH, so the row's
 * buttons wrap under it on a narrow screen. The public-access rows
 * (SynthesizedBlocksPreview) use it too.
 */
export function RuleRowText({
  name,
  readOnly = false,
  action,
  children,
}: {
  name: string;
  /** A rule the page cannot edit: the name is muted, not bold. */
  readOnly?: boolean;
  action: AdmissionBlock['action'];
  children: React.ReactNode;
}) {
  const { TEXT_MUTED } = useColors();
  return (
    <div style={{ flex: `1 1 ${RULE_TEXT_MIN_WIDTH}px`, minWidth: 0 }}>
      <div style={{ display: 'flex', flexWrap: 'wrap', columnGap: 8, rowGap: 2, alignItems: 'center' }}>
        {/* Monospace, broken only where it must (a very long name). */}
        <Text
          strong={!readOnly}
          style={{ fontFamily: 'var(--font-mono)', overflowWrap: 'anywhere', ...(readOnly ? { color: TEXT_MUTED } : {}) }}
        >
          {name}
        </Text>
        <ActionBadge action={action} />
      </div>
      <Text type="secondary" style={{ fontSize: 12, display: 'block', overflowWrap: 'anywhere' }}>
        {children}
      </Text>
    </div>
  );
}

/**
 * The action of a rule as a tag. The AntD preset colours put light green
 * text on a green tint (2.5:1); the theme accents reach 4.5:1 on light and
 * dark surfaces, so the text uses them and only the tint stays pale.
 */
function ActionBadge({ action }: { action: AdmissionBlock['action'] }) {
  const colors = useColors();
  const kind = actionKind(action);
  const colour: Record<string, string> = {
    'allow-anonymous': colors.ACCENT_GREEN,
    deny: colors.ACCENT_RED,
    continue: colors.ACCENT_BLUE,
    reject: colors.ACCENT_AMBER,
  };
  const c = colour[kind] ?? colors.TEXT_SECONDARY;
  const label =
    kind === 'reject' && typeof action !== 'string'
      ? `reject ${action.status}`
      : kind;
  return (
    <Tag style={{ margin: 0, fontSize: 12, color: c, background: `${c}14`, borderColor: `${c}55` }}>
      {label}
    </Tag>
  );
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
