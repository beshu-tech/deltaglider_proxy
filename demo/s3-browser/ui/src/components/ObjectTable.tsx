import type { CSSProperties } from 'react';
import { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { Table, Typography, Alert, Progress, Checkbox, theme, Button, Select } from 'antd';
import { FolderOutlined, FileOutlined, LoadingOutlined, CalculatorOutlined, CloseCircleOutlined, WarningOutlined } from '@ant-design/icons';
import type { S3Object } from '../types';
import { formatBytes, relativeTime } from '../utils';
import type { ColumnsType, TableProps } from 'antd/es/table';
import type { GetRef } from 'antd';
import { useColors } from '../ThemeContext';
import type { FolderSizeState } from '../useComputeSize';
import { folderSizeText, folderSizeTitle } from '../folderSize';
import { getPreviewMode } from './filePreviewMode';
import { canRequestPrefixUsageScan, isVirtualFolderPrefix } from '../permissions';
import { usePersistedPageSize } from '../usePersistedPageSize';
import { clampPageToData, describeVisibleRange } from '../paginationLabels';
import StorageTypeTag from './StorageTypeTag';
import { buildRows, sortRows, type BrowserRow, type SortColumn, type SortState } from '../browserNav';

const { Text } = Typography;

// Static (theme-independent) parts of the column-header title style. The
// theme-dependent `color` is merged in at each use site.
const COL_HEADER_STYLE: CSSProperties = { fontSize: 11, fontWeight: 600, fontFamily: 'var(--font-ui)' };

// Static parts of the monospace value-cell style; `color` is merged per use.
const MONO_CELL_STYLE: CSSProperties = { fontFamily: 'var(--font-mono)', fontSize: 12 };

/**
 * Allowed object-table page sizes, smallest → largest. The S3 LIST
 * pagination returns up to 1000 keys per upstream page, so anything
 * here fits inside a single round trip even when the bucket is well
 * past the in-memory cap (`MAX_LIST_PAGES` in s3client.ts).
 */
const PAGE_SIZE_OPTIONS = [25, 50, 100, 250, 500] as const;
const DEFAULT_PAGE_SIZE = 100;
const PAGE_SIZE_STORAGE_KEY = 'dg-object-table-page-size';

interface Props {
  objects: S3Object[];
  folders: string[];
  prefix: string;
  selected: S3Object | null;
  onSelect: (obj: S3Object) => void;
  onNavigate: (prefix: string) => void;
  selectedKeys: Set<string>;
  onToggleKey: (key: string) => void;
  onToggleAll: () => void;
  isMobile: boolean;
  isTruncated: boolean;
  refreshing: boolean;
  headCache: Record<string, { storageType?: string; storedSize?: number; error?: boolean }>;
  onEnrichKeys: (keys: string[]) => void;
  folderSizes: Record<string, FolderSizeState>;
  virtualFolders: string[];
  hasAdminSession: boolean;
  onComputeSize: (prefix: string) => void;
  onCancelSize: (prefix: string) => void;
  onAutoPopulateSizes?: (currentPrefix: string, folderPrefixes: string[]) => void;
  onPreview?: (obj: S3Object) => void;
  /** rowKey of the keyboard-cursor row (arrow-key navigation), or null. */
  cursorKey?: string | null;
  /** Sync the keyboard cursor when a row is clicked. */
  onCursorChange?: (key: string | null) => void;
  /** Reports the displayed (sorted) row order, so keyboard navigation walks it. */
  onRowOrderChange?: (keys: string[]) => void;
}

type RowData = BrowserRow;

const SORT_COLUMNS: readonly string[] = ['name', 'size', 'modified'] satisfies SortColumn[];

/**
 * Size cell for a folder row: renders the scan state machine
 * (virtual / not-scannable / loading / done / error / idle).
 * Extracted from the Size column render to keep the column config flat.
 */
function FolderSizeCell({
  folderPrefix,
  sizeState,
  canScanFolder,
  isVirtual,
  onComputeSize,
  onCancelSize,
}: {
  folderPrefix: string;
  sizeState: FolderSizeState | undefined;
  canScanFolder: boolean;
  isVirtual: boolean;
  onComputeSize: (prefix: string) => void;
  onCancelSize: (prefix: string) => void;
}) {
  const { TEXT_SECONDARY, TEXT_MUTED } = useColors();

  if (isVirtual) {
    return (
      <span
        title="Virtual folder from your permissions. It will become a real folder after upload."
        style={{ fontSize: 11, color: TEXT_MUTED }}
      >
        Virtual
      </span>
    );
  }
  if (!canScanFolder) {
    return (
      <span style={{ fontSize: 12, color: TEXT_MUTED }} title="Open Settings and sign in as an administrator to show folder size">
        —
      </span>
    );
  }
  if (sizeState?.loading) {
    return (
      <Button
        title={sizeState.progress ? formatBytes(sizeState.progress.totalSize) + ' across ' + sizeState.progress.totalFiles.toLocaleString() + ' files so far...' : 'Starting...'}
        type="text"
        size="small"
        icon={<CloseCircleOutlined />}
        onClick={(e) => { e.stopPropagation(); onCancelSize(folderPrefix); }}
        style={{ ...MONO_CELL_STYLE, fontSize: 11, color: TEXT_SECONDARY, padding: '0 4px', height: 'auto' }}
      >
        <LoadingOutlined style={{ marginRight: 4 }} />
        {sizeState.progress ? formatBytes(sizeState.progress.totalSize) : '...'}
      </Button>
    );
  }
  if (sizeState?.progress?.done) {
    return (
      <span
        title={folderSizeTitle(sizeState.progress.totalFiles, Boolean(sizeState.progress.lowerBound))}
        style={{ ...MONO_CELL_STYLE, color: TEXT_SECONDARY, cursor: 'default' }}
      >
        {folderSizeText(formatBytes(sizeState.progress.totalSize), Boolean(sizeState.progress.lowerBound))}
      </span>
    );
  }
  if (sizeState?.error) {
    return (
      <Button
        title={sizeState.error}
        type="text"
        size="small"
        icon={<CalculatorOutlined />}
        onClick={(e) => { e.stopPropagation(); onComputeSize(folderPrefix); }}
        style={{ fontSize: 11, color: TEXT_MUTED, padding: '0 4px', height: 'auto' }}
      >
        Retry
      </Button>
    );
  }
  return (
    <Button
      type="text"
      size="small"
      icon={<CalculatorOutlined />}
      onClick={(e) => { e.stopPropagation(); onComputeSize(folderPrefix); }}
      style={{ fontSize: 11, color: TEXT_MUTED, padding: '0 4px', height: 'auto' }}
    >
      Size
    </Button>
  );
}

export default function ObjectTable({
  objects,
  folders,
  prefix,
  selected,
  onSelect,
  onNavigate,
  selectedKeys,
  onToggleKey,
  onToggleAll,
  isMobile,
  isTruncated,
  refreshing,
  headCache,
  onEnrichKeys,
  folderSizes,
  virtualFolders,
  hasAdminSession,
  onComputeSize,
  onCancelSize,
  onAutoPopulateSizes,
  onPreview,
  cursorKey = null,
  onCursorChange,
  onRowOrderChange,
}: Props) {
  const { token } = theme.useToken();
  const { TEXT_PRIMARY, TEXT_SECONDARY, TEXT_MUTED, ACCENT_BLUE, ACCENT_AMBER, ACCENT_PURPLE, STORAGE_TYPE_COLORS, STORAGE_TYPE_DEFAULT } = useColors();

  const [pageSize, setPageSize] = usePersistedPageSize(
    PAGE_SIZE_STORAGE_KEY,
    DEFAULT_PAGE_SIZE,
    PAGE_SIZE_OPTIONS,
  );
  const [currentPage, setCurrentPage] = useState(1);
  // Controlled sort: AntD only draws the header state (`sorter: true` never
  // sorts); `sortedRows` below is the one displayed order.
  const [sortState, setSortState] = useState<SortState | null>(null);

  // THE displayed row order, shared by the Table, HEAD enrichment, the cursor
  // page and (via onRowOrderChange) keyboard navigation. Memoised so a 10k-row
  // listing is not rebuilt on every render.
  const rows = useMemo(() => buildRows(folders, objects, prefix), [folders, objects, prefix]);
  const sortedRows = useMemo(
    () => sortRows(rows, sortState, (fp) => folderSizes[fp]?.progress?.totalSize ?? 0),
    [rows, sortState, folderSizes],
  );
  useEffect(() => {
    onRowOrderChange?.(sortedRows.map((r) => r.key));
  }, [sortedRows, onRowOrderChange]);

  // Guard against rapid folder clicks (issue #4)
  const navigatingRef = useRef(false);
  const guardedNavigate = useCallback((p: string) => {
    if (navigatingRef.current) return;
    navigatingRef.current = true;
    onNavigate(p);
    // Reset after a short delay to allow next navigation
    setTimeout(() => { navigatingRef.current = false; }, 300);
  }, [onNavigate]);

  // Reset to page 1 when the data set changes shape: prefix navigation OR a
  // search/filter that shrinks (or grows) the row count. Depending on
  // `objects.length` rather than `objects` avoids a spurious reset on
  // identity-only re-renders (e.g. HEAD enrichment replaces the array but
  // keeps the same count). Without this, a search that drops 500 rows to 50
  // while sitting on page 3 left `currentPage` stale and `enrichPage`
  // computing an out-of-range slice against the shrunken list.
  useEffect(() => { setCurrentPage(1); }, [prefix, objects.length]);

  // Compute visible file keys for the current page (in DISPLAYED order) and
  // request HEAD enrichment.
  const enrichPage = useCallback((page: number, size: number) => {
    // Clamp to the page actually backed by data: when a search shrinks the
    // list, the page-reset effect and this enrich effect run in the same
    // render pass with `currentPage` still stale, so without clamping `start`
    // could land past the end and request keys that no longer exist.
    const safePage = clampPageToData(page, sortedRows.length, size);
    const fileKeys = sortedRows
      .slice((safePage - 1) * size, safePage * size)
      .flatMap((r) => (r._isFolder ? [] : [r.key]));
    if (fileKeys.length > 0) onEnrichKeys(fileKeys);
  }, [sortedRows, onEnrichKeys]);

  // Enrich when page, page-size, sort or objects change
  useEffect(() => {
    if (objects.length > 0) enrichPage(currentPage, pageSize);
  }, [currentPage, pageSize, objects.length, enrichPage]);

  const handleTableChange = useCallback<NonNullable<TableProps<RowData>['onChange']>>(
    (_pagination, _filters, sorter, extra) => {
      if (extra.action !== 'sort' || Array.isArray(sorter)) return;
      const column = String(sorter.columnKey ?? '');
      setSortState(
        sorter.order && SORT_COLUMNS.includes(column)
          ? { column: column as SortColumn, order: sorter.order }
          : null,
      );
    },
    [],
  );
  const sortOrderFor = (column: SortColumn) => (sortState?.column === column ? sortState.order : null);

  // Page-size change resets the operator to page 1 — otherwise
  // "page 5 of 25-per-page" becomes nonsense after switching to 250.
  const handlePageSizeChange = useCallback(
    (next: number) => {
      setPageSize(next);
      setCurrentPage(1);
    },
    [setPageSize],
  );

  // Auto-populate folder sizes from cached scanner results
  useEffect(() => {
    if (folders.length > 0 && onAutoPopulateSizes) {
      onAutoPopulateSizes(prefix, folders);
    }
  }, [prefix, folders, onAutoPopulateSizes]);

  // Scroll container for the keyboard-cursor scroll-into-view (effect below,
  // after `dataSource` is computed).
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const tableRef = useRef<GetRef<typeof Table<RowData>>>(null);

  // Virtualized body height: the Table renders only visible rows (Tier 3.2),
  // which needs a fixed scroll.y in px. The app is a page-scrolling layout
  // (ancestors grow with content, min-height:auto all the way up), so the
  // container's own height is CONTENT-driven — measuring it feeds back into
  // itself and the body balloons to the full row count. Derive the height
  // from the VIEWPORT instead: window height minus the table's viewport
  // offset minus the fixed chrome below the body (header row + pagination +
  // status bar). Content changes can't move that number, only real window /
  // pane geometry changes can.
  const [bodyHeight, setBodyHeight] = useState(400);
  // The pager row renders only when there is more than one page.
  const multiPage = folders.length + objects.length > pageSize;
  useEffect(() => {
    const el = scrollContainerRef.current;
    if (!el) return;
    // 39 thead + 48 pagination (when shown) + 44 status bar + 9 breathing room.
    const CHROME_BELOW = multiPage ? 140 : 92;
    const measure = () => {
      // Clamp a negative top (window resized while the page is scrolled) so
      // the height stays viewport-bounded instead of growing by the scroll.
      const top = Math.max(0, el.getBoundingClientRect().top);
      setBodyHeight(Math.max(240, window.innerHeight - top - CHROME_BELOW));
    };
    measure();
    window.addEventListener('resize', measure);
    // Sidebar collapse / banner appearance moves the table without a window
    // resize — a ResizeObserver on the container's OFFSET parent catches it.
    const ro = new ResizeObserver(measure);
    if (el.parentElement) ro.observe(el.parentElement);
    return () => {
      window.removeEventListener('resize', measure);
      ro.disconnect();
    };
  }, [multiPage]);

  function fileIconColor(name: string): string {
    const ext = name.split('.').pop()?.toLowerCase() || '';
    if (['jpg', 'jpeg', 'png', 'gif', 'svg', 'webp', 'ico', 'bmp'].includes(ext)) return ACCENT_PURPLE;
    if (['zip', 'tar', 'gz', 'bz2', '7z', 'rar', 'xz'].includes(ext)) return ACCENT_AMBER;
    return TEXT_MUTED;
  }

  const dataSource = sortedRows;
  const totalItems = dataSource.length;
  const totalSelectable = totalItems;
  const allChecked = totalSelectable > 0 && selectedKeys.size === totalSelectable;
  const someChecked = selectedKeys.size > 0 && selectedKeys.size < totalSelectable;

  // Keyboard cursor: follow it across pages and scroll its row into view.
  // `dataSource` is the displayed (sorted) order, the same order
  // useBrowserKeyboardNav walks (via onRowOrderChange). Depend on the cursor's INDEX (a number), not the
  // array identity, so the effect only runs when the cursor actually moves to a
  // different row — not on every re-render (which would re-fire the scroll).
  //
  // `currentPage` is deliberately NOT a dep: the effect READS it to decide
  // whether to jump, but must only run when the CURSOR moves — including it
  // would make a manual pager click (which changes currentPage) snap the page
  // back to wherever the cursor sits, so the mouse pager would fight the cursor.
  const cursorIndex = cursorKey ? dataSource.findIndex((r) => r.key === cursorKey) : -1;
  useEffect(() => {
    if (cursorIndex === -1 || !cursorKey) return;
    const targetPage = Math.floor(cursorIndex / pageSize) + 1;
    setCurrentPage((page) => (page === targetPage ? page : targetPage));
    // Defer the scroll so the row exists after any page switch + render.
    // With `virtual`, offscreen rows have no DOM node, so use the table's
    // scrollTo({key}) instead of querySelector + scrollIntoView.
    const id = requestAnimationFrame(() => {
      tableRef.current?.scrollTo?.({ key: cursorKey });
    });
    return () => cancelAnimationFrame(id);
  }, [cursorKey, cursorIndex, pageSize]);

  const columns: ColumnsType<RowData> = [
    {
      title: () => (
        <Checkbox
          checked={allChecked}
          indeterminate={someChecked}
          onChange={onToggleAll}
          aria-label="Select all"
        />
      ),
      key: 'select',
      width: 40,
      // The whole cell is the hit target, and a click here never reaches the
      // row handler (which opens the inspector).
      onCell: (record: RowData) => ({
        onClick: (e: React.MouseEvent) => {
          e.stopPropagation();
          onToggleKey(record.key);
        },
        style: { cursor: 'pointer' },
      }),
      render: (_: unknown, record: RowData) => (
        <Checkbox
          checked={selectedKeys.has(record.key)}
          onClick={(e) => e.stopPropagation()}
          onChange={() => onToggleKey(record.key)}
          aria-label={`Select ${record.name}`}
        />
      ),
    },
    {
      title: () => <span style={{ ...COL_HEADER_STYLE, color: TEXT_MUTED }}>Name</span>,
      dataIndex: 'name',
      key: 'name',
      sorter: true, // ordering lives in sortRows (browserNav.ts)
      sortOrder: sortOrderFor('name'),
      // `ellipsis` sets no title for a rendered (non-string) cell, and AntD
      // tooltips are hidden globally: the cells carry a native `title` instead.
      ellipsis: true,
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) {
          return (
            <button
              className="btn-reset"
              title={record.name}
              onClick={() => guardedNavigate(record.key.replace('folder:', ''))}
              style={{ fontWeight: 500, color: TEXT_PRIMARY, gap: 8, fontFamily: "var(--font-ui)" }}
            >
              <FolderOutlined aria-hidden="true" style={{ color: ACCENT_BLUE, fontSize: 15 }} />
              {record.name}
              {isVirtualFolderPrefix(record.key.replace('folder:', ''), virtualFolders) ? (
                <sup
                  title="Virtual folder from your permissions. It will become a real folder after upload."
                  style={{ color: TEXT_MUTED, fontSize: 10, fontWeight: 600, lineHeight: 1, marginLeft: 2 }}
                >
                  (i)
                </sup>
              ) : null}
            </button>
          );
        }
        return (
          <span data-testid={`object-row-${record.name}`} title={record.name} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <FileOutlined aria-hidden="true" style={{ color: fileIconColor(record.name), fontSize: 14 }} />
            <span style={{ fontFamily: "var(--font-mono)", fontSize: 13, color: TEXT_PRIMARY, cursor: 'pointer', flex: 1 }}>
              {record.name}
            </span>
          </span>
        );
      },
    },
    {
      title: () => <span style={{ ...COL_HEADER_STYLE, color: TEXT_MUTED }}>Size</span>,
      key: 'size',
      width: isMobile ? 80 : 100,
      sorter: true,
      sortOrder: sortOrderFor('size'),
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) {
          const folderPrefix = record.key.replace('folder:', '');
          return (
            <FolderSizeCell
              folderPrefix={folderPrefix}
              sizeState={folderSizes[folderPrefix]}
              canScanFolder={canRequestPrefixUsageScan(folderPrefix, virtualFolders, hasAdminSession)}
              isVirtual={isVirtualFolderPrefix(folderPrefix, virtualFolders)}
              onComputeSize={onComputeSize}
              onCancelSize={onCancelSize}
            />
          );
        }
        return <span style={{ ...MONO_CELL_STYLE, color: TEXT_SECONDARY }}>{formatBytes(record.size)}</span>;
      },
    },
    {
      title: () => <span style={{ ...COL_HEADER_STYLE, color: TEXT_MUTED }}>Modified</span>,
      key: 'modified',
      width: 200,
      responsive: ['lg'] as const,
      sorter: true,
      sortOrder: sortOrderFor('modified'),
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        if (!record.lastModified) return <span style={{ fontSize: 12, color: TEXT_MUTED }}>--</span>;
        const date = new Date(record.lastModified);
        return (
          <span title={date.toLocaleString()} style={{ fontSize: 12, color: TEXT_SECONDARY, cursor: 'default' }}>
            {relativeTime(date)}
          </span>
        );
      },
    },
    {
      title: () => <span style={{ ...COL_HEADER_STYLE, color: TEXT_MUTED }}>Compression</span>,
      key: 'compression',
      width: 130,
      align: 'center' as const,
      responsive: ['sm'] as const,
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        const cached = headCache[record.key];
        if (!cached) return <LoadingOutlined style={{ fontSize: 12, color: TEXT_MUTED }} />;
        if (cached.error) return <WarningOutlined title="Failed to load metadata" style={{ fontSize: 12, color: ACCENT_AMBER }} />;
        return (
          <StorageTypeTag
            storageType={cached.storageType}
            colors={STORAGE_TYPE_COLORS}
            fallback={STORAGE_TYPE_DEFAULT}
          />
        );
      },
    },
  ];

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      {refreshing && (
        <Progress
          percent={100}
          status="active"
          showInfo={false}
          size={{ height: 2 }}
          style={{ lineHeight: 0, marginBottom: 0 }}
        />
      )}
      {/*
        AntD's built-in Pagination size-changer is disabled; we render
        a standalone <Select> for "Rows per page" in the status bar
        below so the range readout and the size picker live together.
      */}
      <div ref={scrollContainerRef} style={{ flex: 1, overflow: 'auto' }}>
        <Table<RowData>
          ref={tableRef}
          virtual
          columns={columns}
          dataSource={dataSource}
          onChange={handleTableChange}
          rowKey="key"
          showSorterTooltip={false} /* Ant Design 6 rc-table renders sort tooltips inline in <th>, causing layout shift */
          pagination={{
            pageSize,
            current: currentPage,
            onChange: (page) => setCurrentPage(page),
            // Size changer disabled — we render our own Select
            // in the status bar below.
            showSizeChanger: false,
            size: 'small',
            // The status bar below owns the range text ("Showing 101–200
            // of 1,500 items · Page 2 of 15"); the pager shows only page
            // buttons, and only when there is a second page. It used to
            // repeat the range at the top AND the bottom.
            hideOnSinglePage: true,
          }}
          size="small"
          /* `virtual` needs a fixed body height and pins the header itself,
             so the old `sticky` + free-flow scroll={undefined} are gone.
             `x` must be numeric in virtual mode (rc-table warns + forces 1
             otherwise): fixed columns sum to ~370, plus a Name minimum. */
          scroll={{ y: bodyHeight, x: 700 }}
          rowClassName={(record) => {
            const classes: string[] = [];
            if (!record._isFolder && selected?.key === record.key) classes.push('ant-table-row-selected');
            if (record.key === cursorKey) classes.push('dg-row-cursor');
            return classes.join(' ');
          }}
          onRow={(record) => ({
            onClick: () => {
              onCursorChange?.(record.key);
              if (!record._isFolder) onSelect(record);
            },
            onDoubleClick: () => {
              if (!record._isFolder && onPreview && getPreviewMode(record.key)) {
                onPreview(record as S3Object);
              }
            },
            style: {
              borderBottom: `1px solid ${token.colorBorderSecondary}`,
              transition: 'background 0.15s ease',
              cursor: !record._isFolder ? 'pointer' : undefined,
            },
          })}
        />
      </div>

      {isTruncated && (
        <Alert
          type="warning"
          showIcon
          banner
          title="Showing first 10,000 objects. Navigate into a folder to see more."
          style={{ borderRadius: 0 }}
        />
      )}

      {/* Status bar — the single source of truth for the visible-range
          summary, mirrored to assistive tech via `aria-live`, so screen
          readers announce page/size changes even when focus is on the
          page-size dropdown. */}
      {/*
        Footer row: aria-live range readout on the left, page-size
        Select on the right.
      */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          gap: 16,
          padding: '8px 20px',
          borderTop: `1px solid ${token.colorBorderSecondary}`,
          flexShrink: 0,
        }}
      >
        <div role="status" aria-live="polite" style={{ flex: 1, minWidth: 0 }}>
          <Text style={{ fontSize: 12, color: TEXT_MUTED, fontFamily: 'var(--font-mono)' }}>
            {describeVisibleRange(totalItems, currentPage, pageSize)}
          </Text>
        </div>
        <label
          style={{
            display: 'inline-flex',
            alignItems: 'center',
            gap: 8,
            fontSize: 12,
            color: TEXT_MUTED,
            fontFamily: 'var(--font-ui)',
            flexShrink: 0,
          }}
        >
          <span>Rows per page</span>
          <Select
            size="small"
            value={String(pageSize)}
            onChange={(v) => {
              const n = Number(v);
              if (Number.isFinite(n)) handlePageSizeChange(n);
            }}
            options={PAGE_SIZE_OPTIONS.map((n) => ({
              value: String(n),
              label: String(n),
            }))}
            style={{ width: 84 }}
          />
        </label>
      </div>
    </div>
  );
}
