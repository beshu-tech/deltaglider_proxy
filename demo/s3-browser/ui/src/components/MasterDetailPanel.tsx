import type { ReactNode } from 'react';
import { Button, Typography, Alert, Input } from 'antd';
import { PlusOutlined, SearchOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { LoadingState } from './StatePlaceholders';

const { Text } = Typography;

/**
 * Generic master-detail scaffolding shared by UsersPanel and GroupsPanel.
 *
 * Owns ONLY the presentational shell + selection/hover interaction:
 *   - the outer column flex + optional `banner` slot
 *   - the fixed-width left list (title, New button, search input, list states)
 *   - per-row selection highlight + hover background chrome
 *   - the right-hand detail pane
 *
 * Data fetching, mutation, and the row/detail bodies stay in the consumer —
 * the panels differ in those and must not be forced into a shared data layer.
 * `renderRowBody` provides the inner row content (the consumer keeps full
 * control of icons/badges/labels/per-row action buttons); this component wraps
 * it with the selection/hover container and stable React key.
 */
interface MasterDetailPanelProps<T> {
  /** Optional banner rendered above the split (e.g. IamSourceBanner). */
  banner?: ReactNode;
  title: string;
  searchPlaceholder: string;

  items: T[];
  getId: (item: T) => number;
  /** True when `item` is the selected row and the panel is not in create mode. */
  isSelected: (item: T) => boolean;
  renderRowBody: (item: T) => ReactNode;
  onSelect: (item: T) => void;
  /** Vertical padding for each row — users use 12px, groups 10px. */
  rowPadding: string;
  /** Optional className applied to each row container (users use "user-list-item"). */
  rowClassName?: string;

  onCreate: () => void;
  /**
   * Read-only mode (declarative IAM): hides the "New" toolbar button. Per-row
   * action buttons live in the consumer's `renderRowBody`, so the consumer is
   * responsible for hiding those too — this flag only governs the shared shell.
   */
  readOnly?: boolean;
  search: string;
  onSearchChange: (value: string) => void;

  loading: boolean;
  error: string;
  /** Rendered in the list area when there are zero items and not loading/error. */
  listEmptyState: ReactNode;

  /** The right-hand detail pane (form-or-empty-state). */
  detail: ReactNode;
}

export default function MasterDetailPanel<T>({
  banner,
  title,
  searchPlaceholder,
  items,
  getId,
  isSelected,
  renderRowBody,
  onSelect,
  rowPadding,
  rowClassName,
  onCreate,
  readOnly = false,
  search,
  onSearchChange,
  loading,
  error,
  listEmptyState,
  detail,
}: MasterDetailPanelProps<T>) {
  const colors = useColors();

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', overflow: 'hidden' }}>
      {banner !== undefined && (
        <div style={{ padding: '12px 16px 0' }}>{banner}</div>
      )}
      <div style={{ display: 'flex', flex: 1, overflow: 'hidden' }}>
        {/* Left: List */}
        <div style={{
          width: 300,
          minWidth: 260,
          borderRight: `1px solid ${colors.BORDER}`,
          display: 'flex',
          flexDirection: 'column',
          overflow: 'hidden',
        }}>
          <div style={{ padding: '16px 16px 12px', borderBottom: `1px solid ${colors.BORDER}` }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 10 }}>
              <Text strong style={{ fontSize: 14 }}>{title}</Text>
              {!readOnly && (
                <Button
                  data-testid={`md-new-${title.toLowerCase().replace(/\s+/g, '-')}`}
                  aria-label={`New ${title.replace(/s$/, '') || title}`}
                  type="primary"
                  size="small"
                  icon={<PlusOutlined />}
                  onClick={onCreate}
                >
                  New
                </Button>
              )}
            </div>
            <Input
              prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
              placeholder={searchPlaceholder}
              value={search}
              onChange={e => onSearchChange(e.target.value)}
              allowClear
              size="small"
              style={{ borderRadius: 6 }}
            />
          </div>

          <div style={{ flex: 1, overflow: 'auto', padding: '4px 0' }}>
            {loading && items.length === 0 && <LoadingState />}
            {error && (
              <Alert type="error" message={error} showIcon style={{ margin: 8, borderRadius: 8 }} />
            )}
            {!loading && items.length === 0 && !error && listEmptyState}
            {items.map(item => {
              const selected = isSelected(item);
              return (
                <div
                  key={getId(item)}
                  onClick={() => onSelect(item)}
                  className={rowClassName}
                  style={{
                    padding: rowPadding,
                    cursor: 'pointer',
                    background: selected ? colors.ACCENT_BLUE + '18' : 'transparent',
                    borderLeft: selected ? `3px solid ${colors.ACCENT_BLUE}` : '3px solid transparent',
                    transition: 'all 0.15s ease',
                    position: 'relative',
                  }}
                  onMouseEnter={e => { if (!selected) e.currentTarget.style.background = colors.BORDER + '40'; }}
                  onMouseLeave={e => { if (!selected) e.currentTarget.style.background = 'transparent'; }}
                >
                  {renderRowBody(item)}
                </div>
              );
            })}
          </div>
        </div>

        {/* Right: Detail */}
        <div style={{ flex: 1, overflow: 'auto', background: colors.BG_CARD }}>
          {detail}
        </div>
      </div>
    </div>
  );
}
