import { Table, Tag, Progress, Typography } from 'antd';
import { FolderOutlined } from '@ant-design/icons';
import type { S3Object } from '../types';
import { formatBytes, savingsPercent, displayName } from '../utils';
import type { ColumnsType } from 'antd/es/table';

const { Link } = Typography;

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
}

type RowData = { _isFolder: true; key: string; name: string } | (S3Object & { _isFolder: false; name: string });

const storageColors: Record<string, string> = {
  reference: 'blue',
  delta: 'purple',
  direct: 'default',
};

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
}: Props) {
  const allChecked = objects.length > 0 && selectedKeys.size === objects.length;

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

  const columns: ColumnsType<RowData> = [
    {
      title: 'Name',
      dataIndex: 'name',
      key: 'name',
      sorter: (a, b) => a.name.localeCompare(b.name),
      ellipsis: isMobile,
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) {
          return (
            <span
              onClick={() => onNavigate(record.key.replace('folder:', ''))}
              style={{ cursor: 'pointer', fontWeight: 500 }}
            >
              <FolderOutlined style={{ marginRight: 8 }} />
              {record.name}
            </span>
          );
        }
        return (
          <Link
            onClick={() => onSelect(record)}
            style={{ fontFamily: 'monospace', fontSize: isMobile ? 12 : 13 }}
          >
            {record.name}
          </Link>
        );
      },
    },
    {
      title: 'Size',
      key: 'size',
      width: isMobile ? 70 : 100,
      sorter: (a, b) => {
        const sa = a._isFolder ? -1 : a.size;
        const sb = b._isFolder ? -1 : b.size;
        return sa - sb;
      },
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        return <span style={{ fontFamily: 'monospace', fontSize: isMobile ? 11 : undefined }}>{formatBytes(record.size)}</span>;
      },
    },
    {
      title: 'Type',
      key: 'type',
      width: 100,
      responsive: ['sm'] as const,
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        const st = record.storageType;
        if (!st) return null;
        return <Tag color={storageColors[st] || 'default'}>{st}</Tag>;
      },
    },
    {
      title: 'Stored',
      key: 'stored',
      width: 100,
      responsive: ['lg'] as const,
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        return (
          <span style={{ fontFamily: 'monospace' }}>
            {record.storedSize != null ? formatBytes(record.storedSize) : '--'}
          </span>
        );
      },
    },
    {
      title: 'Savings',
      key: 'savings',
      width: 140,
      responsive: ['lg'] as const,
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        const savings = savingsPercent(record);
        if (savings == null) return '--';
        return (
          <Progress
            percent={Math.round(savings)}
            size="small"
            strokeColor={savings > 50 ? '#52c41a' : savings > 20 ? '#1890ff' : undefined}
          />
        );
      },
    },
    {
      title: 'Modified',
      key: 'modified',
      width: 180,
      responsive: ['md'] as const,
      sorter: (a, b) => {
        const da = a._isFolder ? '' : a.lastModified || '';
        const db = b._isFolder ? '' : b.lastModified || '';
        return da.localeCompare(db);
      },
      render: (_: unknown, record: RowData) => {
        if (record._isFolder) return null;
        return record.lastModified ? new Date(record.lastModified).toLocaleString() : '--';
      },
    },
  ];

  const rowSelection = {
    selectedRowKeys: Array.from(selectedKeys),
    onChange: (keys: React.Key[]) => {
      const newSet = new Set(keys.map(String));
      for (const k of keys) {
        const ks = String(k);
        if (!selectedKeys.has(ks)) onToggleKey(ks);
      }
      for (const k of selectedKeys) {
        if (!newSet.has(k)) onToggleKey(k);
      }
    },
    getCheckboxProps: (record: RowData) => ({
      disabled: record._isFolder,
      style: record._isFolder ? { display: 'none' } : undefined,
    }),
    selections: [
      {
        key: 'all',
        text: allChecked ? 'Deselect all' : 'Select all',
        onSelect: () => onToggleAll(),
      },
    ],
    columnWidth: isMobile ? 32 : 48,
  };

  return (
    <Table<RowData>
      columns={columns}
      dataSource={dataSource}
      rowKey="key"
      rowSelection={rowSelection}
      pagination={false}
      size="small"
      sticky
      scroll={isMobile ? { x: 'max-content' } : undefined}
      rowClassName={(record) => {
        if (!record._isFolder && selected?.key === record.key) return 'ant-table-row-selected';
        return '';
      }}
      onRow={(record) => ({
        style: record._isFolder ? { cursor: 'pointer' } : undefined,
        onClick: record._isFolder
          ? () => onNavigate(record.key.replace('folder:', ''))
          : undefined,
      })}
    />
  );
}
