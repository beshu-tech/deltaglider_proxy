import { useState } from 'react';
import { Drawer, Tabs, Descriptions, Tag, Button, Space, Typography, Dropdown, message, Modal, Input } from 'antd';
import { DownloadOutlined, DeleteOutlined, LinkOutlined, ApiOutlined } from '@ant-design/icons';
import { deleteObject, downloadObject, getPresignedUrl, getObjectUrl } from '../s3client';
import { formatBytes } from '../utils';
import type { S3Object } from '../types';

const { Text } = Typography;

interface Props {
  object: S3Object | null;
  onClose: () => void;
  onDeleted: () => void;
  isMobile?: boolean;
}

const SKIP_HEADERS = new Set([
  'content-length', 'content-type', 'etag', 'last-modified',
  'x-amz-storage-type', 'x-deltaglider-stored-size',
  'date', 'vary', 'access-control-allow-origin', 'access-control-expose-headers',
]);

function getDgMetadata(headers: Record<string, string>): [string, string][] {
  return Object.entries(headers)
    .filter(([k]) => k.startsWith('x-amz-meta-dg-'))
    .map(([k, v]) => [k.replace('x-amz-meta-dg-', ''), v]);
}

function getUserMetadata(headers: Record<string, string>): [string, string][] {
  return Object.entries(headers)
    .filter(([k]) => k.startsWith('x-amz-meta-') && !k.startsWith('x-amz-meta-dg-'))
    .map(([k, v]) => [k.replace('x-amz-meta-', ''), v]);
}

function getOtherHeaders(headers: Record<string, string>): [string, string][] {
  return Object.entries(headers)
    .filter(([k]) => !SKIP_HEADERS.has(k) && !k.startsWith('x-amz-meta-'))
    .sort(([a], [b]) => a.localeCompare(b));
}

function KVList({ items }: { items: [string, string][] }) {
  if (items.length === 0) {
    return <Text type="secondary" italic>None</Text>;
  }
  return (
    <Descriptions column={1} size="small" bordered>
      {items.map(([k, v]) => (
        <Descriptions.Item key={k} label={k}>
          <Text code style={{ fontSize: 12, wordBreak: 'break-all' }}>{v}</Text>
        </Descriptions.Item>
      ))}
    </Descriptions>
  );
}

export default function InspectorPanel({ object, onClose, onDeleted, isMobile }: Props) {
  const [activeTab, setActiveTab] = useState('properties');
  const [messageApi, contextHolder] = message.useMessage();

  if (!object) return null;

  const savings =
    object.storedSize != null && object.size > 0
      ? ((1 - object.storedSize / object.size) * 100).toFixed(1)
      : null;

  const dgMeta = getDgMetadata(object.headers);
  const userMeta = getUserMetadata(object.headers);
  const otherHeaders = getOtherHeaders(object.headers);

  const handleDelete = async () => {
    await deleteObject(object.key);
    onClose();
    onDeleted();
  };

  const handleDownload = async () => {
    const blob = await downloadObject(object.key);
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = object.key.split('/').pop() || 'download';
    a.click();
    URL.revokeObjectURL(url);
  };

  const generateUrl = async (): Promise<string> => {
    try {
      return await getPresignedUrl(object.key);
    } catch (e) {
      console.warn('Presigned URL failed, falling back to direct URL:', e);
      return getObjectUrl(object.key);
    }
  };

  const handleCopyLink = async () => {
    try {
      const url = await generateUrl();
      await navigator.clipboard.writeText(url);
      messageApi.success('Link copied');
    } catch {
      messageApi.error('Failed to copy link');
    }
  };

  const handleCreateLink = async () => {
    try {
      const url = await generateUrl();
      Modal.info({
        title: 'Object URL',
        width: 520,
        content: (
          <Input.TextArea
            value={url}
            autoSize={{ minRows: 3, maxRows: 6 }}
            readOnly
            style={{ marginTop: 8, fontFamily: 'monospace', fontSize: 12 }}
            onFocus={(e) => e.target.select()}
          />
        ),
        okText: 'Close',
      });
    } catch {
      messageApi.error('Failed to generate URL');
    }
  };

  const storageColors: Record<string, string> = {
    reference: 'blue',
    delta: 'purple',
    direct: 'default',
  };

  const downloadMenuItems = [
    {
      key: 'copyLink',
      icon: <LinkOutlined />,
      label: 'Copy link (7 days)',
      onClick: handleCopyLink,
    },
    {
      key: 'createLink',
      icon: <ApiOutlined />,
      label: 'Create link',
      onClick: handleCreateLink,
    },
  ];

  const tabItems = [
    {
      key: 'properties',
      label: 'Properties',
      children: (
        <Descriptions column={1} size="small" bordered>
          <Descriptions.Item label="Key">
            <Text code style={{ fontSize: 12, wordBreak: 'break-all' }}>{object.key}</Text>
          </Descriptions.Item>
          <Descriptions.Item label="Original Size">
            <Text code>{formatBytes(object.size)}</Text>
          </Descriptions.Item>
          <Descriptions.Item label="Content-Type">
            <Text code>{object.headers['content-type'] || '--'}</Text>
          </Descriptions.Item>
          <Descriptions.Item label="Storage Type">
            {object.storageType ? (
              <Tag color={storageColors[object.storageType] || 'default'}>{object.storageType}</Tag>
            ) : '--'}
          </Descriptions.Item>
          <Descriptions.Item label="Stored Size">
            <Text code>{object.storedSize != null ? formatBytes(object.storedSize) : '--'}</Text>
          </Descriptions.Item>
          <Descriptions.Item label="Savings">
            <Text strong style={{ color: savings ? '#52c41a' : undefined }}>
              {savings != null ? `${savings}%` : '--'}
            </Text>
          </Descriptions.Item>
          <Descriptions.Item label="ETag">
            <Text code style={{ fontSize: 12, wordBreak: 'break-all' }}>{object.etag}</Text>
          </Descriptions.Item>
          <Descriptions.Item label="Last Modified">
            <Text code>{object.lastModified ? new Date(object.lastModified).toLocaleString() : '--'}</Text>
          </Descriptions.Item>
        </Descriptions>
      ),
    },
    {
      key: 'metadata',
      label: 'Metadata',
      children: (
        <Space direction="vertical" style={{ width: '100%' }} size={16}>
          {dgMeta.length > 0 && (
            <div>
              <Text strong style={{ fontSize: 12, marginBottom: 8, display: 'block' }}>DeltaGlider</Text>
              <KVList items={dgMeta} />
            </div>
          )}
          <div>
            <Text strong style={{ fontSize: 12, marginBottom: 8, display: 'block' }}>User Metadata</Text>
            <KVList items={userMeta} />
          </div>
        </Space>
      ),
    },
    {
      key: 'headers',
      label: 'Headers',
      children: (
        <div>
          <Text strong style={{ fontSize: 12, marginBottom: 8, display: 'block' }}>Response Headers</Text>
          <KVList items={otherHeaders} />
        </div>
      ),
    },
  ];

  return (
    <>
      {contextHolder}
      <Drawer
        title={object.key.split('/').pop()}
        placement="right"
        width={isMobile ? '100%' : 400}
        open={!!object}
        onClose={onClose}
        footer={
          <Space style={{ width: '100%' }}>
            <Dropdown.Button
              type="primary"
              icon={<DownloadOutlined />}
              menu={{ items: downloadMenuItems }}
              onClick={handleDownload}
            >
              <DownloadOutlined /> Download
            </Dropdown.Button>
            <Button icon={<DeleteOutlined />} danger onClick={handleDelete}>
              Delete
            </Button>
          </Space>
        }
      >
        <Tabs activeKey={activeTab} onChange={setActiveTab} items={tabItems} />
      </Drawer>
    </>
  );
}
