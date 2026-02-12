import { useState, useEffect } from 'react';
import { Typography, theme } from 'antd';
import { CloudUploadOutlined } from '@ant-design/icons';

const { Text, Title } = Typography;

interface Props {
  onDrop: (files: FileList) => void;
  prefix: string;
}

export default function DropZone({ onDrop, prefix }: Props) {
  const [dragging, setDragging] = useState(false);
  const { token } = theme.useToken();

  useEffect(() => {
    let dragCount = 0;

    const onDragEnter = (e: DragEvent) => {
      e.preventDefault();
      dragCount++;
      if (dragCount === 1) setDragging(true);
    };

    const onDragLeave = (e: DragEvent) => {
      e.preventDefault();
      dragCount--;
      if (dragCount === 0) setDragging(false);
    };

    const onDragOver = (e: DragEvent) => {
      e.preventDefault();
    };

    const onDropHandler = (e: DragEvent) => {
      e.preventDefault();
      dragCount = 0;
      setDragging(false);
      if (e.dataTransfer?.files.length) {
        onDrop(e.dataTransfer.files);
      }
    };

    document.addEventListener('dragenter', onDragEnter);
    document.addEventListener('dragleave', onDragLeave);
    document.addEventListener('dragover', onDragOver);
    document.addEventListener('drop', onDropHandler);
    return () => {
      document.removeEventListener('dragenter', onDragEnter);
      document.removeEventListener('dragleave', onDragLeave);
      document.removeEventListener('dragover', onDragOver);
      document.removeEventListener('drop', onDropHandler);
    };
  }, [onDrop]);

  if (!dragging) return null;

  return (
    <div
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 1000,
        background: token.colorBgMask,
        backdropFilter: 'blur(4px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
      }}
    >
      <div
        style={{
          border: `2px dashed ${token.colorPrimary}`,
          borderRadius: 16,
          padding: '64px',
          textAlign: 'center',
          maxWidth: 500,
        }}
      >
        <CloudUploadOutlined style={{ fontSize: 48, color: token.colorPrimary, marginBottom: 16 }} />
        <Title level={4}>Drop files to upload</Title>
        <Text type="secondary">
          to <Text code>{prefix || '/'}</Text>
        </Text>
      </div>
    </div>
  );
}
