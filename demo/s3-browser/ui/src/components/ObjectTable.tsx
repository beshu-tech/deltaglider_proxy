import type { CSSProperties, ReactNode } from 'react';
import { useState, useEffect, useLayoutEffect, useCallback, useRef, useMemo, useId } from 'react';
import { Table, Typography, Alert, Progress, Checkbox, theme, Button, Select, Tag } from 'antd';
import { FolderOutlined, FileOutlined, LoadingOutlined, CalculatorOutlined, CloseCircleOutlined } from '@ant-design/icons';
import type { S3Object } from '../types';
import { formatBytes, relativeTime } from '../utils';
import type { ColumnsType, TableProps } from 'antd/es/table';
import { withKeyboardSort } from '../tableSort';
import type { GetRef } from 'antd';
import { useColors } from '../ThemeContext';
import type { FolderSizeState } from '../useComputeSize';
import { folderSizeText, folderSizeTitle } from '../folderSize';
import { getPreviewMode } from './filePreviewMode';
import { canRequestPrefixUsageScan, isVirtualFolderPrefix } from '../permissions';
import { usePersistedPageSize } from '../usePersistedPageSize';
import { describeVisibleRange } from '../paginationLabels';
import StorageTypeTag from './StorageTypeTag';
import MiddleEllipsis from '../browser/MiddleEllipsis';
import { storageChip } from '../deltaSummary';
import { buildRows, sortRows, type BrowserRow, type SortColumn, type SortState } from '../browserNav';
import { useIsNarrow } from '../useIsNarrow';

/** Below this width the Size column folds into a line under the name: the
 *  name column alone did not fit next to it on a phone. */
const SIZE_SUBLINE_BELOW = 720;
/** The minimum table width (fixed columns ~370 + a Name minimum) on wider screens. */
const TABLE_MIN_WIDTH = 700;

const { Text } = Typography;

/** The header of a `virtual` table is a real <table>. Inside the grid its
 *  implicit table/rowgroup roles would nest a second table, so it is
 *  presentational; its <tr>/<th> carry explicit row/columnheader roles. */
function PresentationTable(props: React.HTMLAttributes<HTMLTableElement>) {
  return <table {...props} role="presentation" />;
}
const GRID_COMPONENTS = { header: { table: PresentationTable } };

/** Grid roles on every cell, and `aria-sort` on the sorted header. */
function withGridRoles<T>(columns: ColumnsType<T>, sort: SortState | null): ColumnsType<T> {
  return columns.map((col) => {
    const ownCell = col.onCell;
    const ownHeader = col.onHeaderCell;
    const sorted = sort && col.key === sort.column ? (sort.order === 'ascend' ? 'ascending' : 'descending') : undefined;
    return {
      ...col,
      onCell: (record: T, index?: number) => ({ ...(ownCell?.(record, index) ?? {}), role: 'gridcell' }),
      onHeaderCell: (c: Parameters<NonNullable<typeof ownHeader>>[0]) => ({
        ...(ownHeader?.(c) ?? {}),
        role: 'columnheader',
        ...(sorted ? { 'aria-sort': sorted } : {}),
      }),
    };
  }) as ColumnsType<T>;
}

/** The body row of `key` under `root`, if it is rendered (virtual rows
 *  outside the viewport are not). */
function rowElement(root: HTMLElement, key: string): HTMLElement | null {
  const esc = typeof CSS !== 'undefined' && CSS.escape ? CSS.escape(key) : key.replace(/["\\]/g, '\\$&');
  return root.querySelector<HTMLElement>(`.ant-table-row[data-row-key="${esc}"]`);
}

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

/** The folder's rows and listing status. */
interface ObjectTableListing {
  objects: S3Object[];
  folders: string[];
  prefix: string;
  isTruncated: boolean;
  /** When the rows after the first listing page were listed: the 60 s
   *  auto-refresh reads page 1 only. Null = the folder fits one page. */
  restListedAt?: number | null;
  refreshing: boolean;
  headCache: Record<string, { storageType?: string; storedSize?: number; isBaseline?: boolean }>;
  /** Folders implied by writable prefixes only (never scanned). */
  virtualFolders: string[];
}

/** The inspected row and the checked rows. */
interface ObjectTableSelection {
  /** The object open in the inspector (highlighted row). */
  inspected: S3Object | null;
  selectedKeys: Set<string>;
  onToggleKey: (key: string) => void;
  onToggleAll: () => void;
}

/** Row activation: a click opens, a folder navigates, a double-click previews. */
interface ObjectTableActions {
  onSelect: (obj: S3Object) => void;
  onNavigate: (prefix: string) => void;
  onPreview?: (obj: S3Object) => void;
}

/** The folder Size column (admin usage scanner). */
export interface ObjectTableFolderSizes {
  sizes: Record<string, FolderSizeState>;
  /** An admin session may scan; otherwise the column shows a dash. */
  canScan: boolean;
  onCompute: (prefix: string) => void;
  onCancel: (prefix: string) => void;
  onAutoPopulate?: (currentPrefix: string, folderPrefixes: string[]) => void;
}

/** Keyboard cursor (useBrowserKeyboardNav). */
export interface ObjectTableCursor {
  /** rowKey of the cursor row, or null. */
  key: string | null;
  /** Sync the cursor when a row is clicked. */
  onChange?: (key: string | null) => void;
  /** Reports the displayed (sorted) row order, so keyboard navigation walks it. */
  onRowOrderChange?: (keys: string[]) => void;
}

interface Props {
  listing: ObjectTableListing;
  selection: ObjectTableSelection;
  actions: ObjectTableActions;
  folderSizes: ObjectTableFolderSizes;
  cursor?: ObjectTableCursor;
  isMobile: boolean;
  /** Free space (px) kept under the rows: a floating bar sits there instead
   *  of over the last row. */
  reserveBottom?: number;
  /** Shown in the footer after the range text (the bucket totals on a
   *  narrow window, where the header has no room for them). */
  footerExtra?: ReactNode;
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
  // While a scan runs the cell is a spinner; pointing at it (or focusing it)
  // turns the spinner into the cancel icon. Both at once read as noise.
  const [cancelShown, setCancelShown] = useState(false);

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
        title={`${sizeState.progress ? `${formatBytes(sizeState.progress.totalSize)} across ${sizeState.progress.totalFiles.toLocaleString()} files so far` : 'Counting'}. Click to stop.`}
        aria-label="Stop counting the folder size"
        type="text"
        size="small"
        icon={cancelShown ? <CloseCircleOutlined /> : <LoadingOutlined />}
        onMouseEnter={() => setCancelShown(true)}
        onMouseLeave={() => setCancelShown(false)}
        onFocus={() => setCancelShown(true)}
        onBlur={() => setCancelShown(false)}
        onClick={(e) => { e.stopPropagation(); onCancelSize(folderPrefix); }}
        style={{ ...MONO_CELL_STYLE, fontSize: 11, color: TEXT_SECONDARY, padding: '0 4px', height: 'auto' }}
      >
        {sizeState.progress ? formatBytes(sizeState.progress.totalSize) : null}
      </Button>
    );
  }
  if (sizeState?.progress?.done) {
    return (
      <span
        title={folderSizeTitle(sizeState.progress.totalFiles, sizeState.progress.bound)}
        style={{ ...MONO_CELL_STYLE, color: TEXT_SECONDARY, cursor: 'default' }}
      >
        {folderSizeText(formatBytes(sizeState.progress.totalSize), sizeState.progress.bound)}
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
  listing,
  selection,
  actions,
  folderSizes: sizeProps,
  cursor,
  isMobile,
  reserveBottom = 0,
  footerExtra,
}: Props) {
  const { objects, folders, prefix, isTruncated, restListedAt = null, refreshing, headCache, virtualFolders } = listing;
  const { inspected: selected, selectedKeys, onToggleKey, onToggleAll } = selection;
  const { onSelect, onNavigate, onPreview } = actions;
  const {
    sizes: folderSizes,
    canScan: hasAdminSession,
    onCompute: onComputeSize,
    onCancel: onCancelSize,
    onAutoPopulate: onAutoPopulateSizes,
  } = sizeProps;
  const cursorKey = cursor?.key ?? null;
  const onCursorChange = cursor?.onChange;
  const onRowOrderChange = cursor?.onRowOrderChange;
  const uid = useId();
  const { token } = theme.useToken();
  const { TEXT_PRIMARY, TEXT_SECONDARY, TEXT_MUTED, ACCENT_BLUE, ACCENT_AMBER, ACCENT_PURPLE, STORAGE_TYPE_COLORS, STORAGE_TYPE_DEFAULT } = useColors();

  const [pageSize, setPageSize] = usePersistedPageSize(
    PAGE_SIZE_STORAGE_KEY,
    DEFAULT_PAGE_SIZE,
    PAGE_SIZE_OPTIONS,
  );
  const [currentPage, setCurrentPage] = useState(1);
  const sizeSubline = useIsNarrow(SIZE_SUBLINE_BELOW);
  // Controlled sort: AntD only draws the header state (`sorter: true` never
  // sorts); `sortedRows` below is the one displayed order.
  const [sortState, setSortState] = useState<SortState | null>(null);

  // THE displayed row order, shared by the Table, the cursor
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
  // identity-only re-renders (e.g. a refresh replaces the array but keeps
  // the same count). Without this, a search that drops 500 rows to 50 while
  // sitting on page 3 left `currentPage` stale.
  useEffect(() => { setCurrentPage(1); }, [prefix, objects.length]);

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

  // Auto-populate folder sizes from cached scanner results: one request per
  // distinct folder list. The effect matches on the list's content, not its
  // identity, so a parent that rebuilds the array each render cannot turn it
  // into a request loop.
  const foldersKey = folders.join('\n');
  const foldersRef = useRef(folders);
  foldersRef.current = folders;
  useEffect(() => {
    const current = foldersRef.current;
    if (current.length > 0 && onAutoPopulateSizes) {
      onAutoPopulateSizes(prefix, current);
    }
  }, [prefix, foldersKey, onAutoPopulateSizes]);

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
  // Narrow screens: the table is exactly as wide as its container, so no
  // horizontal scrollbar strip shows under the rows.
  const [containerWidth, setContainerWidth] = useState(0);
  // The pager row renders only when there is more than one page.
  const multiPage = folders.length + objects.length > pageSize;
  useEffect(() => {
    const el = scrollContainerRef.current;
    if (!el) return;
    // 39 thead + 48 pagination (when shown) + 44 status bar + 9 breathing room.
    const CHROME_BELOW = (multiPage ? 140 : 92) + reserveBottom;
    const measure = () => {
      // Clamp a negative top (window resized while the page is scrolled) so
      // the height stays viewport-bounded instead of growing by the scroll.
      const top = Math.max(0, el.getBoundingClientRect().top);
      setBodyHeight(Math.max(240, window.innerHeight - top - CHROME_BELOW));
      setContainerWidth(Math.floor(el.clientWidth));
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
  }, [multiPage, reserveBottom]);

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
      // Roving focus: when a row has the focus, the focus follows the
      // cursor, so a screen reader announces the row the arrows moved to.
      const root = scrollContainerRef.current;
      const active = document.activeElement;
      if (root && active instanceof HTMLElement && active.classList.contains('ant-table-row') && root.contains(active)) {
        requestAnimationFrame(() => rowElement(root, cursorKey)?.focus({ preventScroll: true }));
      }
    });
    return () => cancelAnimationFrame(id);
  }, [cursorKey, cursorIndex, pageSize]);

  // The grid root is AntD's `.ant-table` (the pager sits outside it, and a
  // list is not allowed inside a grid). AntD offers no prop for its
  // attributes, so they are set after each render.
  const gridLabel = `Objects in ${prefix || 'the bucket root'}`;
  useLayoutEffect(() => {
    const root = scrollContainerRef.current?.querySelector('.ant-table');
    if (!root) return;
    root.setAttribute('role', 'grid');
    root.setAttribute('aria-label', gridLabel);
    root.setAttribute('aria-rowcount', String(totalItems + 1));
    root.setAttribute('aria-multiselectable', 'true');
  });
  const pageStart = (currentPage - 1) * pageSize;
  // The row that Tab reaches: the cursor, else the first row of the page.
  const tabStopKey = cursorKey ?? dataSource[pageStart]?.key ?? null;

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
              style={{ fontWeight: 500, color: TEXT_PRIMARY, gap: 8, fontFamily: "var(--font-ui)", maxWidth: '100%', minWidth: 0 }}
            >
              <FolderOutlined aria-hidden="true" style={{ color: ACCENT_BLUE, fontSize: 15, flexShrink: 0 }} />
              <MiddleEllipsis text={record.name} title={false} />
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
          <span data-testid={`object-row-${record.name}`} title={record.name} style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
            <FileOutlined aria-hidden="true" style={{ color: fileIconColor(record.name), fontSize: 14, flexShrink: 0 }} />
            <span style={{ display: 'flex', flexDirection: 'column', minWidth: 0, flex: 1, cursor: 'pointer' }}>
              {/* Cut in the middle: versions of one file differ at the end. */}
              <MiddleEllipsis
                text={record.name}
                title={false}
                style={{ fontFamily: "var(--font-mono)", fontSize: 13, color: TEXT_PRIMARY }}
              />
              {sizeSubline && (
                <span data-testid="object-size-subline" style={{ fontSize: 11, color: TEXT_MUTED, fontFamily: 'var(--font-ui)', lineHeight: 1.3 }}>
                  {formatBytes(record.size)}
                  {record.lastModified ? ` · ${relativeTime(new Date(record.lastModified))}` : ''}
                </span>
              )}
            </span>
          </span>
        );
      },
    },
    // A number column: right-aligned, the folder Size action too (it sat
    // left in it and read as a stray chip).
    ...(sizeSubline ? [] : [{
      title: () => <span style={{ ...COL_HEADER_STYLE, color: TEXT_MUTED }}>Size</span>,
      key: 'size',
      width: isMobile ? 80 : 110,
      align: 'right' as const,
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
        return <span style={{ ...MONO_CELL_STYLE, color: TEXT_SECONDARY, fontVariantNumeric: 'tabular-nums' }}>{formatBytes(record.size)}</span>;
      },
    }]),
    {
      title: () => <span style={{ ...COL_HEADER_STYLE, color: TEXT_MUTED }}>Modified</span>,
      key: 'modified',
      // "12 min ago" fits; the freed width goes to the name.
      width: 130,
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
        // From the listing; a row without facts shows the neutral tag. The
        // folder's baseline is "Baseline", as the inspector calls it.
        const facts = headCache[record.key];
        const chip = storageChip(facts?.storageType, Boolean(facts?.isBaseline));
        if (chip.label === 'Baseline') {
          return (
            <Tag
              title={chip.title}
              style={{
                background: 'transparent', border: `1px solid ${token.colorBorder}`, color: TEXT_MUTED,
                borderRadius: 6, fontFamily: 'var(--font-mono)', fontSize: 11, fontWeight: 500,
              }}
            >
              {chip.label}
            </Tag>
          );
        }
        return (
          <StorageTypeTag
            storageType={facts?.storageType}
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
          aria-label="Refreshing the listing"
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
          columns={withGridRoles(withKeyboardSort(columns), sortState)}
          components={GRID_COMPONENTS}
          onHeaderRow={() => ({ role: 'row', 'aria-rowindex': 1 }) as React.HTMLAttributes<HTMLElement>}
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
          scroll={{ y: bodyHeight, x: sizeSubline && containerWidth > 0 ? containerWidth : TABLE_MIN_WIDTH }}
          rowClassName={(record) => {
            const classes: string[] = [];
            if (!record._isFolder && selected?.key === record.key) classes.push('ant-table-row-selected');
            if (record.key === cursorKey) classes.push('dg-row-cursor');
            return classes.join(' ');
          }}
          onRow={(record, index) => ({
            role: 'row',
            'aria-rowindex': pageStart + (index ?? 0) + 2,
            'aria-selected': selectedKeys.has(record.key),
            tabIndex: record.key === tabStopKey ? 0 : -1,
            onFocus: (e: React.FocusEvent<HTMLElement>) => {
              if (e.target === e.currentTarget && record.key !== cursorKey) onCursorChange?.(record.key);
            },
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
        {reserveBottom > 0 && <div aria-hidden="true" style={{ height: reserveBottom }} />}
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
        <div style={{ flex: 1, minWidth: 0, display: 'flex', alignItems: 'center', gap: 16, flexWrap: 'wrap' }}>
        <div role="status" aria-live="polite" style={{ minWidth: 0 }}>
          <Text style={{ fontSize: 12, color: TEXT_MUTED, fontFamily: 'var(--font-ui)', fontVariantNumeric: 'tabular-nums' }}>
            {describeVisibleRange(totalItems, currentPage, pageSize)}
            {restListedAt !== null && (
              <span title="The automatic refresh updates the first 1,000 entries only. Refresh updates all of them.">
                {` · entries after the first 1,000 as of ${new Date(restListedAt).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`}
              </span>
            )}
          </Text>
        </div>
        {footerExtra}
        </div>
        {/* The page size matters only when a folder can fill a page. */}
        {totalItems > PAGE_SIZE_OPTIONS[0] && (
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
          <span id={`${uid}-rows`}>Rows per page</span>
          <Select
            aria-labelledby={`${uid}-rows`}
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
        )}
      </div>
    </div>
  );
}
