import type { ReactNode } from 'react';
import { Button, Typography } from 'antd';
import { PlusOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import SectionHeader from './SectionHeader';
import { useCardStyles } from './shared-styles';

const { Text } = Typography;

/**
 * Generic master-detail scaffold for the storage rule-array panels (Lifecycle
 * and Replication). It owns the two-column layout, the selectable rule list,
 * the "Add rule" button, and the empty-vs-detail card switch. The per-rule
 * field set and list-row body stay panel-specific via render props.
 *
 * Selection lives in the parent (it has to, since the selected index is
 * derived from the section-editor's loaded rules and kept in sync there);
 * this component is a pure presentation shell over `rules` + `selectedIndex`.
 *
 * Identity is by ARRAY INDEX, not by name. A rule's name is mutable (the
 * detail editor renames it live, per keystroke) and is not guaranteed unique
 * mid-edit, so keying selection/mutations on the name let a rename-to-collide
 * silently retarget the wrong rule. The index is stable across renames; only
 * add/remove shift it, and both reset the selection in the parent. The rule
 * name is rendered by the panel's own `renderListItem`/`renderDetail`, so this
 * scaffold never needs to read it.
 */
interface RuleListEditorProps<TRule> {
  rules: TRule[];
  selectedIndex: number | null;
  onSelect: (index: number) => void;
  onAdd: () => void;
  /** Section icon + the "N configured rule(s)" subtitle text. */
  icon: ReactNode;
  loading: boolean;
  /** Compact body for a list row (status tag + scope lines). */
  renderListItem: (rule: TRule, index: number) => ReactNode;
  /** Detail editor for the selected rule (fields + action buttons + runtime). */
  renderDetail: (rule: TRule, index: number) => ReactNode;
  /** Shown in the detail card when no rule is selected. */
  emptyState: ReactNode;
  /** Master-list column width (panels differ slightly). */
  listColumn?: string;
}

export default function RuleListEditor<TRule>({
  rules,
  selectedIndex,
  onSelect,
  onAdd,
  icon,
  loading,
  renderListItem,
  renderDetail,
  emptyState,
  listColumn = '320px',
}: RuleListEditorProps<TRule>) {
  const colors = useColors();
  const { cardStyle } = useCardStyles();
  // Resolve the selected index, falling back to the first rule. `null` /
  // out-of-range (e.g. just after a delete) collapses to "no selection".
  const activeIndex =
    selectedIndex != null && selectedIndex >= 0 && selectedIndex < rules.length
      ? selectedIndex
      : rules.length > 0
        ? 0
        : null;
  const selectedRule = activeIndex != null ? rules[activeIndex] : null;

  return (
    <div style={{ display: 'grid', gridTemplateColumns: `${listColumn} minmax(0, 1fr)`, gap: 16 }}>
      <div style={cardStyle}>
        <SectionHeader
          icon={icon}
          title="Rules"
          description={loading ? 'Loading...' : `${rules.length} configured rule${rules.length === 1 ? '' : 's'}.`}
        />
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
          {rules.map((rule, index) => {
            const active = activeIndex === index;
            return (
              <button
                key={index}
                onClick={() => onSelect(index)}
                style={{
                  textAlign: 'left',
                  border: `1px solid ${active ? colors.ACCENT_BLUE : colors.BORDER}`,
                  borderRadius: 10,
                  padding: 12,
                  background: active ? `${colors.ACCENT_BLUE}12` : colors.BG_ELEVATED,
                  cursor: 'pointer',
                }}
              >
                {renderListItem(rule, index)}
              </button>
            );
          })}
          <Button icon={<PlusOutlined />} type="dashed" onClick={onAdd} block>
            Add rule
          </Button>
        </div>
      </div>

      <div style={cardStyle}>
        {selectedRule == null || activeIndex == null
          ? emptyState
          : renderDetail(selectedRule, activeIndex)}
      </div>
    </div>
  );
}

/** Shared list-row title line: bold rule name + a right-aligned status node. */
export function RuleRowTitle({ name, status }: { name: string; status: ReactNode }) {
  return (
    <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8 }}>
      <Text strong style={{ fontSize: 13 }}>{name}</Text>
      {status}
    </div>
  );
}

/** Shared list-row secondary line (11px secondary text, small top margin). */
export function RuleRowLine({ marginTop = 4, children }: { marginTop?: number; children: ReactNode }) {
  return (
    <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop }}>
      {children}
    </Text>
  );
}
