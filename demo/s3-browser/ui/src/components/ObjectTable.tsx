import type { CSSProperties } from 'react';
import { useState, useEffect, useCallback, useRef } from 'react';
import { Table, Typography, Alert, Progress, Checkbox, theme, Button, Select } from 'antd';
import { FolderOutlined, FileOutlined, LoadingOutlined, CalculatorOutlined, CloseCircleOutlined, WarningOutlined } from '@ant-design/icons';
import type { S3Object } from '../types';
import { formatBytes, displayName, timeAgo } from '../utils';
import type { ColumnsType } from 'antd/es/table';
import { useColors } from '../ThemeContext';
import type { FolderSizeState } from '../useComputeSize';
import { getPreviewMode } from './filePreviewMode';
import { canRequestPrefixUsageScan, isVirtualFolderPrefix } from '../permissions';
import { usePersistedPageSize } from '../usePersistedPageSize';
import { clampPageToData, describeVisibleRange } from '../paginationLabels';
import StorageTypeTag from './StorageTypeTag';

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
}

type RowData = { _isFolder: true; key: string; name: string } | (S3Object & { _isFolder: false; name: string });

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
        title={sizeState.progress ? formatBytes(sizeState.progress.totalSize) + ' stored across ' + sizeState.progress.totalFiles.toLocaleString() + ' files so far...' : 'Starting...'}
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
        title={`${sizeState.progress.totalFiles.toLocaleString()} files — stored (compressed) size`}
        style={{ ...MONO_CELL_STYLE, color: TEXT_SECONDARY, cursor: 'default' }}
      >
        {formatBytes(sizeState.progress.totalSize)}
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
}: Props) {
  const { token } = theme.useToken();
  const { TEXT_PRIMARY, TEXT_SECONDARY, TEXT_MUTED, ACCENT_BLUE, ACCENT_AMBER, ACCENT_PURPLE, STORAGE_TYPE_COLORS, STORAGE_TYPE_DEFAULT } = useColors();

  const [pageSize, setPageSize] = usePersistedPageSize(
    PAGE_SIZE_STORAGE_KEY,
    DEFAULT_PAGE_SIZE,
    PAGE_SIZE_OPTIONS,
  );
  const [currentPage, setCurrentPage] = useState(1);

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

  // Compute visible file keys for the current page and request HEAD enrichment
  const enrichPage = useCallback((page: number, size: number) => {
    const folderRows = folders.length;
    const allRows = folderRows + objects.length;
    // Clamp to the page actually backed by data: when a search shrinks the
    // list, the page-reset effect and this enrich effect run in the same
    // render pass with `currentPage` still stale, so without clamping `start`
    // could land past the end and request keys that no longer exist.
    const safePage = clampPageToData(page, allRows, size);
    const start = (safePage - 1) * size;
    const end = Math.min(safePage * size, allRows);
    const fileKeys: string[] = [];
    for (let i = start; i < end; i++) {
      if (i >= folderRows) {
        fileKeys.push(objects[i - folderRows].key);
      }
    }
    if (fileKeys.length > 0) onEnrichKeys(fileKeys);
  }, [folders.length, objects, onEnrichKeys]);

  // Enrich when page or page-size changes or objects load
  useEffect(() => {
    if (objects.length > 0) enrichPage(currentPage, pageSize);
  }, [currentPage, pageSize, objects, enrichPage]);

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

  function fileIconColor(name: string): string {
    const ext = name.split('.').pop()?.toLowerCase() || '';
    if (['jpg', 'jpeg', 'png', 'gif', 'svg', 'webp', 'ico', 'bmp'].includes(ext)) return ACCENT_PURPLE;
    if (['zip', 'tar', 'gz', 'bz2', '7z', 'rar', 'xz'].includes(ext)) return ACCENT_AMBER;
    return TEXT_MUTED;
  }

  const folderRows: RowData[] = folders.map((f) => ({
    _isFolder: true as const,
    key: `folder:${f}`,
    name: displayName(f, prefix),
  }));

  const objectRows: RowData[] = objects.map((obj) => ({
    ...obj,
    _isFolder: false as const,
    name: displayName(obj.key, prefix),
  }));

  const dataSource = [...folderRows, ...objectRows];
  const totalItems = dataSource.length;
  const totalSelectable = totalItems;
  const allChecked = totalSelectable > 0 && selectedKeys.size === totalSelectable;
  const someChecked = selectedKeys.size > 0 && selectedKeys.size < totalSelectable;

  // Keyboard cursor: follow it across pages and scroll its row into view.
  // `dataSource` is folders-then-objects, matching the cursor's row ordering in
  // useBrowserKeyboardNav. Depend on the cursor's INDEX (a number), not the
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
    const id = requestAnimationFrame(() => {
      scrollContainerRef.current
        ?.querySelector(`[data-row-key="${CSS.escape(cursorKey)}"]`)
        ?.scrollIntoView({ block: 'nearest' });
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
      render: (_: unknown, record: RowData) => (
        <Checkbox
          checked={selectedKeys.has(record.key)}
          onChange={() => onToggleKey(record.key)}
          aria-label={`Select ${record.name}`}
        />
      ),
    },
    {
      title: () => <span style={{ ...COL_HEADER_STYLE, color: TEXT_MUTED }}>Name</span>,
      dataIndex: 'name',
      key: 'name',
      sorter: (a, b) => a.name.localeCompare(b.name),
      ellipsis: true,
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) {
          return (
            <button
              className="btn-reset"
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
          <span style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
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
      sorter: (a, b) => {
        // Use the scanned folder size when available so sorting matches
        // what the user sees rendered. Folders without a scanned size
        // sort to 0 (after small files, before larger ones) — better
        // than the old `-1` constant that made every folder tie.
        const rowSize = (r: RowData): number => {
          if (!r._isFolder) return r.size;
          const folderPrefix = r.key.replace('folder:', '');
          return folderSizes[folderPrefix]?.progress?.totalSize ?? 0;
        };
        return rowSize(a) - rowSize(b);
      },
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
      sorter: (a, b) => {
        const da = a._isFolder ? '' : a.lastModified || '';
        const db = b._isFolder ? '' : b.lastModified || '';
        return da.localeCompare(db);
      },
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        if (!record.lastModified) return <span style={{ fontSize: 12, color: TEXT_MUTED }}>--</span>;
        const date = new Date(record.lastModified);
        return (
          <span title={date.toLocaleString()} style={{ fontSize: 12, color: TEXT_SECONDARY, cursor: 'default' }}>
            {timeAgo(date)}
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
          strokeWidth={2}
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
          columns={columns}
          dataSource={dataSource}
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
            // Keep pagination bar visible even when one page fits all
            // (so operators see "X items" with no pager) — the size
            // changer is rendered separately, so this only affects
            // the prev/next buttons + page numbers.
            hideOnSinglePage: false,
            showTotal: (totalCount, range) =>
              `${range[0].toLocaleString()}–${range[1].toLocaleString()} of ${totalCount.toLocaleString()}`,
          }}
          size="small"
          sticky
          scroll={undefined}
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
          message="Showing first 10,000 objects. Navigate into a folder to see more."
          style={{ borderRadius: 0 }}
        />
      )}

      {/* Status bar — single source of truth for the visible-range
          summary, mirrored to assistive tech via `aria-live`. AntD's
          pagination `showTotal` puts the same range next to the page
          buttons; the status bar repeats it so screen readers
          announce page/size changes even when the user's focus is on
          the page-size dropdown. */}
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
