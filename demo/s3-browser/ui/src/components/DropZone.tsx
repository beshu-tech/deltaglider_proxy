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
      role="dialog"
      aria-label="Drop files to upload"
      style={{
        position: 'fixed',
        inset: 0,
        zIndex: 1000,
        background: 'var(--overlay-bg)',
        backdropFilter: 'blur(8px)',
        WebkitBackdropFilter: 'blur(8px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
      }}
    >
      <div
        style={{
          border: `2px dashed ${token.colorPrimary}`,
          borderRadius: 20,
          padding: '64px',
          textAlign: 'center',
          maxWidth: 500,
          animation: 'dropGlow 2s ease-in-out infinite',
          background: 'var(--drop-glow)',
        }}
      >
        <CloudUploadOutlined aria-hidden="true" style={{ fontSize: 56, color: token.colorPrimary, marginBottom: 16 }} />
        <Title level={4} style={{ fontFamily: "var(--font-ui)", fontWeight: 700 }}>Drop files to upload</Title>
        <Text type="secondary" style={{ fontFamily: "var(--font-mono)", fontSize: 13 }}>
          to <Text code>{prefix || '/'}</Text>
        </Text>
      </div>
    </div>
  );
}
