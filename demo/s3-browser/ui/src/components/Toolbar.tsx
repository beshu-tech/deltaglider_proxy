import { useRef } from 'react';
import { Space, Button, theme } from 'antd';
import { UploadOutlined, ReloadOutlined, DeleteOutlined } from '@ant-design/icons';

interface Props {
  onRefresh: () => void;
  onUploadFiles: (files: FileList) => void;
  uploading: boolean;
  selectedCount: number;
  deleting: boolean;
  onBulkDelete: () => void;
  isMobile: boolean;
}

export default function Toolbar({
  onRefresh,
  onUploadFiles,
  uploading,
  selectedCount,
  deleting,
  onBulkDelete,
  isMobile,
}: Props) {
  const inputRef = useRef<HTMLInputElement>(null);
  const { token } = theme.useToken();

  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 8,
        padding: isMobile ? '8px 12px' : '10px 24px',
        borderBottom: `1px solid ${token.colorBorderSecondary}`,
        flexShrink: 0,
        flexWrap: 'wrap',
      }}
    >
      <Space size={8} wrap>
        <Button
          icon={<UploadOutlined />}
          onClick={() => inputRef.current?.click()}
          loading={uploading}
          size={isMobile ? 'small' : 'middle'}
        >
          {uploading ? 'Uploading...' : isMobile ? 'Upload' : 'Upload'}
        </Button>
        <input
          ref={inputRef}
          type="file"
          multiple
          style={{ display: 'none' }}
          onChange={(e) => e.target.files && onUploadFiles(e.target.files)}
        />
        <Button
          icon={<ReloadOutlined />}
          onClick={onRefresh}
          size={isMobile ? 'small' : 'middle'}
        >
          {isMobile ? '' : 'Refresh'}
        </Button>
      </Space>

      <div style={{ flex: 1 }} />

      {selectedCount > 0 && (
        <Button
          danger
          icon={<DeleteOutlined />}
          onClick={onBulkDelete}
          loading={deleting}
          size={isMobile ? 'small' : 'middle'}
        >
          {deleting ? 'Deleting...' : isMobile ? `Delete (${selectedCount})` : `Delete ${selectedCount} selected`}
        </Button>
      )}
    </div>
  );
}
