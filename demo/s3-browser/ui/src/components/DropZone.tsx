import { useState, useEffect } from 'react';
import { Typography, theme } from 'antd';
import { CloudUploadOutlined } from '@ant-design/icons';
import { collectDroppedFiles } from '../droppedFiles';
import { SPACE } from './shared-styles';

const { Text, Title } = Typography;

interface Props {
  onDrop: (files: File[]) => void;
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
      dragCount = Math.max(0, dragCount - 1);
      if (dragCount === 0) setDragging(false);
    };

    const onDragOver = (e: DragEvent) => {
      e.preventDefault();
    };

    const onDropHandler = (e: DragEvent) => {
      e.preventDefault();
      dragCount = 0;
      setDragging(false);
      if (!e.dataTransfer) return;
      // collectDroppedFiles MUST be invoked synchronously (the DataTransfer
      // items are only readable during the event); it walks dropped FOLDERS
      // into their real files, then resolves async.
      collectDroppedFiles(e.dataTransfer).then((files) => {
        if (files.length > 0) onDrop(files);
      });
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
        zIndex: 999,
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
          padding: `${SPACE.xl * 2}px`,
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
