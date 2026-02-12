import { useState, useEffect, useRef } from 'react';
import { Menu, Statistic, Progress, Button, Space, Typography, Input, Drawer, theme, message, Popconfirm } from 'antd';
import { HomeOutlined, FolderOutlined, UploadOutlined, DatabaseOutlined, PlusOutlined, DeleteOutlined } from '@ant-design/icons';
import { getStats, listBuckets, createBucket, deleteBucket, getBucket, setBucket } from '../s3client';
import { formatBytes } from '../utils';
import type { StorageStats, BucketInfo } from '../types';
import DemoDataGenerator from './DemoDataGenerator';

const { Text } = Typography;

interface Props {
  folders: string[];
  prefix: string;
  onNavigate: (prefix: string) => void;
  onUploadFiles: (files: FileList) => void;
  uploading: boolean;
  onMutate: () => void;
  refreshTrigger: number;
  onBucketChange: (bucket: string) => void;
  open: boolean;
  onClose: () => void;
}

export default function Sidebar({
  folders,
  prefix,
  onNavigate,
  onUploadFiles,
  uploading,
  onMutate,
  refreshTrigger,
  onBucketChange,
  open,
  onClose,
}: Props) {
  const [stats, setStats] = useState<StorageStats | null>(null);
  const [buckets, setBuckets] = useState<BucketInfo[]>([]);
  const [newBucketName, setNewBucketName] = useState('');
  const [creatingBucket, setCreatingBucket] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const { token } = theme.useToken();
  const [messageApi, contextHolder] = message.useMessage();

  useEffect(() => {
    getStats()
      .then(setStats)
      .catch(() => setStats(null));
  }, [refreshTrigger]);

  useEffect(() => {
    listBuckets()
      .then(setBuckets)
      .catch(() => setBuckets([]));
  }, [refreshTrigger]);

  const topFolders = folders.map((f) => {
    const display = f.startsWith(prefix) ? f.slice(prefix.length) : f;
    return { path: f, name: display.replace(/\/$/, '') };
  });

  const handleCreateBucket = async () => {
    const name = newBucketName.trim();
    if (!name) return;
    setCreatingBucket(true);
    try {
      await createBucket(name);
      setNewBucketName('');
      messageApi.success(`Bucket "${name}" created`);
      const updated = await listBuckets();
      setBuckets(updated);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      messageApi.error(`Failed to create bucket: ${msg}`);
    } finally {
      setCreatingBucket(false);
    }
  };

  const handleDeleteBucket = async (name: string) => {
    try {
      await deleteBucket(name);
      messageApi.success(`Bucket "${name}" deleted`);
      const updated = await listBuckets();
      setBuckets(updated);
      if (getBucket() === name && updated.length > 0) {
        setBucket(updated[0].name);
        onBucketChange(updated[0].name);
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      messageApi.error(`Failed to delete bucket: ${msg}`);
    }
  };

  const handleSelectBucket = (name: string) => {
    setBucket(name);
    onBucketChange(name);
  };

  const activeBucket = getBucket();

  const bucketMenuItems = buckets.map((b) => ({
    key: b.name,
    icon: <DatabaseOutlined />,
    label: (
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', width: '100%' }}>
        <span style={{ fontFamily: 'monospace', fontSize: 12 }}>{b.name}</span>
        {b.name !== activeBucket && (
          <Popconfirm
            title={`Delete bucket "${b.name}"?`}
            description="Bucket must be empty."
            onConfirm={(e) => { e?.stopPropagation(); handleDeleteBucket(b.name); }}
            onCancel={(e) => e?.stopPropagation()}
            okText="Delete"
            okType="danger"
          >
            <Button
              type="text"
              size="small"
              danger
              icon={<DeleteOutlined />}
              onClick={(e) => e.stopPropagation()}
              style={{ marginLeft: 4 }}
            />
          </Popconfirm>
        )}
      </div>
    ),
  }));

  const folderMenuItems = [
    {
      key: '',
      icon: <HomeOutlined />,
      label: 'All Objects',
    },
    ...topFolders.map(({ path, name }) => ({
      key: path,
      icon: <FolderOutlined />,
      label: <span style={{ fontFamily: 'monospace', fontSize: 12 }}>{name}</span>,
    })),
  ];

  const sidebarContent = (
    <>
      {contextHolder}

      {/* Bucket section */}
      <div style={{ borderBottom: `1px solid ${token.colorBorderSecondary}` }}>
        <div style={{ padding: '12px 16px 4px' }}>
          <Text
            type="secondary"
            strong
            style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 1 }}
          >
            Buckets
          </Text>
        </div>
        <Menu
          mode="inline"
          selectedKeys={[activeBucket]}
          items={bucketMenuItems}
          onClick={({ key }) => handleSelectBucket(key)}
          style={{ borderInlineEnd: 'none' }}
        />
        <div style={{ padding: '4px 16px 12px' }}>
          <Space.Compact style={{ width: '100%' }}>
            <Input
              size="small"
              placeholder="New bucket..."
              value={newBucketName}
              onChange={(e) => setNewBucketName(e.target.value)}
              onPressEnter={handleCreateBucket}
            />
            <Button
              size="small"
              icon={<PlusOutlined />}
              onClick={handleCreateBucket}
              loading={creatingBucket}
            />
          </Space.Compact>
        </div>
      </div>

      {/* Folder navigation */}
      <Menu
        mode="inline"
        selectedKeys={[prefix]}
        items={folderMenuItems}
        onClick={({ key }) => onNavigate(key)}
        style={{ borderInlineEnd: 'none' }}
      />

      {/* Storage stats */}
      <div style={{ padding: '16px', borderTop: `1px solid ${token.colorBorderSecondary}` }}>
        <Text type="secondary" strong style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 1 }}>
          Storage
        </Text>
        <div style={{ marginTop: 12 }}>
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            <Statistic
              title="Objects"
              value={stats ? stats.total_objects : '--'}
              valueStyle={{ fontSize: 14 }}
              style={{ flex: 1, minWidth: 80 }}
            />
            <Statistic
              title="Original"
              value={stats ? formatBytes(stats.total_original_size) : '--'}
              valueStyle={{ fontSize: 14 }}
              style={{ flex: 1, minWidth: 80 }}
            />
          </div>
          <div style={{ display: 'flex', gap: 8, marginTop: 8, flexWrap: 'wrap' }}>
            <Statistic
              title="Stored"
              value={stats ? formatBytes(stats.total_stored_size) : '--'}
              valueStyle={{ fontSize: 14 }}
              style={{ flex: 1, minWidth: 80 }}
            />
            <Statistic
              title="Savings"
              value={stats ? `${stats.savings_percentage.toFixed(1)}%` : '--'}
              valueStyle={{ fontSize: 14, color: token.colorSuccess }}
              style={{ flex: 1, minWidth: 80 }}
            />
          </div>
          {stats && (
            <Progress
              percent={Math.round(stats.savings_percentage)}
              size="small"
              strokeColor={token.colorSuccess}
              style={{ marginTop: 8 }}
            />
          )}
        </div>
      </div>

      {/* Actions */}
      <div style={{ padding: '0 16px 16px', borderTop: `1px solid ${token.colorBorderSecondary}` }}>
        <Text
          type="secondary"
          strong
          style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 1, display: 'block', padding: '12px 0 8px' }}
        >
          Actions
        </Text>
        <Space direction="vertical" style={{ width: '100%' }} size={8}>
          <Button
            icon={<UploadOutlined />}
            block
            onClick={() => inputRef.current?.click()}
            loading={uploading}
          >
            {uploading ? 'Uploading...' : 'Upload Files'}
          </Button>
          <input
            ref={inputRef}
            type="file"
            multiple
            style={{ display: 'none' }}
            onChange={(e) => e.target.files && onUploadFiles(e.target.files)}
          />
          <DemoDataGenerator onDone={onMutate} />
        </Space>
      </div>
    </>
  );

  return (
    <Drawer
      placement="left"
      width={300}
      open={open}
      onClose={onClose}
      styles={{ body: { padding: 0 } }}
    >
      {sidebarContent}
    </Drawer>
  );
}
