import { useState, useEffect, useCallback, useRef } from 'react';
import { Table, Tag, Typography, Alert, Progress, Checkbox, theme, Button } from 'antd';
import { FolderOutlined, FileOutlined, LoadingOutlined, CalculatorOutlined, CloseCircleOutlined, WarningOutlined } from '@ant-design/icons';
import type { S3Object } from '../types';
import { formatBytes, displayName, timeAgo } from '../utils';
import type { ColumnsType } from 'antd/es/table';
import { useColors } from '../ThemeContext';
import type { FolderSizeState } from '../useComputeSize';
import { getPreviewMode } from './FilePreview';

const { Text } = Typography;

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
  onComputeSize: (prefix: string) => void;
  onCancelSize: (prefix: string) => void;
  onAutoPopulateSizes?: (currentPrefix: string, folderPrefixes: string[]) => void;
  onPreview?: (obj: S3Object) => void;
}

type RowData = { _isFolder: true; key: string; name: string } | (S3Object & { _isFolder: false; name: string });

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
  onComputeSize,
  onCancelSize,
  onAutoPopulateSizes,
  onPreview,
}: Props) {
  const { token } = theme.useToken();
  const { TEXT_PRIMARY, TEXT_SECONDARY, TEXT_MUTED, ACCENT_BLUE, ACCENT_AMBER, ACCENT_PURPLE, STORAGE_TYPE_COLORS, STORAGE_TYPE_DEFAULT } = useColors();

  const PAGE_SIZE = 100;
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

  // Reset to page 1 when data changes (prefix navigation, search, etc.)
  useEffect(() => { setCurrentPage(1); }, [prefix]);

  // Compute visible file keys for the current page and request HEAD enrichment
  const enrichPage = useCallback((page: number) => {
    const folderRows = folders.length;
    const allRows = folderRows + objects.length;
    const start = (page - 1) * PAGE_SIZE;
    const end = Math.min(page * PAGE_SIZE, allRows);
    const fileKeys: string[] = [];
    for (let i = start; i < end; i++) {
      if (i >= folderRows) {
        fileKeys.push(objects[i - folderRows].key);
      }
    }
    if (fileKeys.length > 0) onEnrichKeys(fileKeys);
  }, [folders.length, objects, onEnrichKeys]);

  // Enrich when page changes or objects load
  useEffect(() => {
    if (objects.length > 0) enrichPage(currentPage);
  }, [currentPage, objects, enrichPage]);

  // Auto-populate folder sizes from cached scanner results
  useEffect(() => {
    if (folders.length > 0 && onAutoPopulateSizes) {
      onAutoPopulateSizes(prefix, folders);
    }
  }, [prefix, folders, onAutoPopulateSizes]);

  function fileIconColor(name: string): string {
    const ext = name.split('.').pop()?.toLowerCase() || '';
    if (['jpg', 'jpeg', 'png', 'gif', 'svg', 'webp', 'ico', 'bmp'].includes(ext)) return ACCENT_PURPLE;
    if (['zip', 'tar', 'gz', 'bz2', '7z', 'rar', 'xz'].includes(ext)) return ACCENT_AMBER;
    return TEXT_MUTED;
  }

  function compressionTag(storageType?: string) {
    const label = !storageType || storageType === 'passthrough' ? 'Original'
      : storageType.charAt(0).toUpperCase() + storageType.slice(1);
    const c = STORAGE_TYPE_COLORS[storageType || 'passthrough'] || STORAGE_TYPE_DEFAULT;
    return (
      <Tag style={{
        background: c.bg,
        border: `1px solid ${c.border}`,
        color: c.text,
        borderRadius: 6,
        fontFamily: "var(--font-mono)",
        fontSize: 11,
        fontWeight: 500,
      }}>
        {label}
      </Tag>
    );
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
      title: () => <span style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Name</span>,
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
      title: () => <span style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Size</span>,
      key: 'size',
      width: isMobile ? 80 : 100,
      sorter: (a, b) => {
        const sa = a._isFolder ? -1 : a.size;
        const sb = b._isFolder ? -1 : b.size;
        return sa - sb;
      },
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) {
          const folderPrefix = record.key.replace('folder:', '');
          const sizeState = folderSizes[folderPrefix];
          if (sizeState?.loading) {
            return (
              <Button
                title={sizeState.progress ? formatBytes(sizeState.progress.totalSize) + ' stored across ' + sizeState.progress.totalFiles.toLocaleString() + ' files so far...' : 'Starting...'}
                type="text"
                size="small"
                icon={<CloseCircleOutlined />}
                onClick={(e) => { e.stopPropagation(); onCancelSize(folderPrefix); }}
                style={{ fontSize: 11, fontFamily: "var(--font-mono)", color: TEXT_SECONDARY, padding: '0 4px', height: 'auto' }}
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
                style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: TEXT_SECONDARY, cursor: 'default' }}
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
        return <span style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: TEXT_SECONDARY }}>{formatBytes(record.size)}</span>;
      },
    },
    {
      title: () => <span style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Modified</span>,
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
      title: () => <span style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Compression</span>,
      key: 'compression',
      width: 130,
      align: 'center' as const,
      responsive: ['sm'] as const,
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        const cached = headCache[record.key];
        if (!cached) return <LoadingOutlined style={{ fontSize: 12, color: TEXT_MUTED }} />;
        if (cached.error) return <WarningOutlined title="Failed to load metadata" style={{ fontSize: 12, color: ACCENT_AMBER }} />;
        return compressionTag(cached.storageType);
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
      <div style={{ flex: 1, overflow: 'auto' }}>
        <Table<RowData>
          columns={columns}
          dataSource={dataSource}
          rowKey="key"
          showSorterTooltip={false} /* Ant Design 6 rc-table renders sort tooltips inline in <th>, causing layout shift */
          pagination={{
            pageSize: PAGE_SIZE,
            current: currentPage,
            onChange: (page) => setCurrentPage(page),
            showSizeChanger: false,
            size: 'small',
            hideOnSinglePage: true,
          }}
          size="small"
          sticky
          scroll={undefined}
          rowClassName={(record) => {
            if (!record._isFolder && selected?.key === record.key) return 'ant-table-row-selected';
            return '';
          }}
          onRow={(record) => ({
            onClick: () => {
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

      {/* Status bar */}
      <div
        role="status"
        aria-live="polite"
        style={{
          padding: '10px 20px',
          borderTop: `1px solid ${token.colorBorderSecondary}`,
          flexShrink: 0,
        }}
      >
        <Text style={{ fontSize: 12, color: TEXT_MUTED, fontFamily: "var(--font-mono)" }}>
          {totalItems} {totalItems === 1 ? 'item' : 'items'}
        </Text>
      </div>
    </div>
  );
}
