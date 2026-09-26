import { CONTENT_WIDE } from './shared-styles';
import type { ReactNode } from 'react';
import { Button, Typography, Alert, Input } from 'antd';
import { PlusOutlined, SearchOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { LoadingState } from './StatePlaceholders';
import './MasterDetailPanel.css';
import { activateOnKey } from '../keyboard';

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
  /**
   * Per-row buttons (Duplicate…). Rendered NEXT TO the row's own button, not
   * inside it: a button inside role="button" is unreachable for assistive
   * technology (axe nested-interactive).
   */
  renderRowActions?: (item: T) => ReactNode;
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
  renderRowActions,
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
    <div
      className="dg-master-detail"
      style={
        {
          display: 'flex',
          flexDirection: 'column',
          height: '100%',
          overflow: 'hidden',
          // The shared wide content column, like every other admin page.
          width: '100%',
          maxWidth: CONTENT_WIDE,
          margin: '0 auto',
          '--dg-md-border': colors.BORDER,
        } as React.CSSProperties
      }
    >
      {banner !== undefined && (
        <div style={{ padding: '12px 16px 0' }}>{banner}</div>
      )}
      {/* Split: side-by-side on a wide container, stacked (list-over-detail) on
          narrow via the container query in MasterDetailPanel.css. */}
      <div className="dg-md-split" style={{ display: 'flex', flex: 1, overflow: 'hidden' }}>
        {/* Left: List */}
        <div
          className="dg-md-list"
          style={{
            width: 300,
            minWidth: 260,
            borderRight: `1px solid ${colors.BORDER}`,
            display: 'flex',
            flexDirection: 'column',
            overflow: 'hidden',
          }}
        >
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
              <Alert type="error" title={error} showIcon style={{ margin: 8, borderRadius: 8 }} />
            )}
            {!loading && items.length === 0 && !error && listEmptyState}
            {items.map(item => {
              const selected = isSelected(item);
              const actions = renderRowActions?.(item);
              return (
                <div
                  key={getId(item)}
                  className={rowClassName}
                  style={{
                    background: selected ? colors.ACCENT_BLUE + '18' : 'transparent',
                    borderLeft: selected ? `3px solid ${colors.ACCENT_BLUE}` : '3px solid transparent',
                    transition: 'all 0.15s ease',
                    position: 'relative',
                  }}
                  onMouseEnter={e => { if (!selected) e.currentTarget.style.background = colors.BORDER + '40'; }}
                  onMouseLeave={e => { if (!selected) e.currentTarget.style.background = 'transparent'; }}
                >
                  <div
                    role="button"
                    tabIndex={0}
                    aria-current={selected || undefined}
                    onClick={() => onSelect(item)}
                    onKeyDown={activateOnKey(() => onSelect(item))}
                    style={{ padding: rowPadding, paddingRight: actions ? 40 : undefined, cursor: 'pointer' }}
                  >
                    {renderRowBody(item)}
                  </div>
                  {actions && <div style={{ position: 'absolute', top: 8, right: 8 }}>{actions}</div>}
                </div>
              );
            })}
          </div>
        </div>

        {/* Right: Detail */}
        <div className="dg-md-detail" style={{ flex: 1, overflow: 'auto', background: colors.BG_CARD }}>
          {detail}
        </div>
      </div>
    </div>
  );
}
