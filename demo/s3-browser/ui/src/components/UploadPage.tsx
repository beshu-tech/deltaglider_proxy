import { useState, useRef, useEffect } from 'react';
import { Button, Typography, Input, Space, Modal } from 'antd';
import {
  CloudUploadOutlined,
  FolderAddOutlined,
  ArrowLeftOutlined,
  CheckCircleFilled,
  CloseCircleFilled,
  LoadingOutlined,
  DeleteOutlined,
} from '@ant-design/icons';
import { getBucket } from '../s3client';
import { formatBytes } from '../utils';
import useUploadQueue from '../useUploadQueue';
import { useColors } from '../ThemeContext';

const { Text, Title } = Typography;

interface Props {
  prefix: string;
  onBack: () => void;
  onDone: () => void;
}

export default function UploadPage({ prefix, onBack, onDone }: Props) {
  const {
    BG_BASE, BG_ELEVATED, BORDER, TEXT_PRIMARY,
    TEXT_SECONDARY, TEXT_MUTED, ACCENT_BLUE, ACCENT_GREEN, ACCENT_RED, ACCENT_PURPLE,
  } = useColors();
  const [destination, setDestination] = useState(prefix);
  const [dragging, setDragging] = useState(false);
  const [folderModalOpen, setFolderModalOpen] = useState(false);
  const [folderName, setFolderName] = useState('');
  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);
  const dropRef = useRef<HTMLDivElement>(null);

  const bucket = getBucket();
  const { queue, stats, savings, pendingCount, addFiles, clearCompleted } = useUploadQueue(destination);

  useEffect(() => {
    const el = dropRef.current;
    if (!el) return;
    let dragCount = 0;

    const onDragEnter = (e: DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      dragCount++;
      if (dragCount === 1) setDragging(true);
    };
    const onDragLeave = (e: DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      dragCount--;
      if (dragCount === 0) setDragging(false);
    };
    const onDragOver = (e: DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
    };
    const onDrop = (e: DragEvent) => {
      e.preventDefault();
      e.stopPropagation();
      dragCount = 0;
      setDragging(false);
      if (e.dataTransfer?.files.length) {
        addFiles(e.dataTransfer.files);
      }
    };

    el.addEventListener('dragenter', onDragEnter);
    el.addEventListener('dragleave', onDragLeave);
    el.addEventListener('dragover', onDragOver);
    el.addEventListener('drop', onDrop);
    return () => {
      el.removeEventListener('dragenter', onDragEnter);
      el.removeEventListener('dragleave', onDragLeave);
      el.removeEventListener('dragover', onDragOver);
      el.removeEventListener('drop', onDrop);
    };
  }, [addFiles]);

  const handleBack = () => {
    onDone();
    onBack();
  };

  const handleNewFolder = () => {
    setFolderName('');
    setFolderModalOpen(true);
  };

  const handleFolderConfirm = () => {
    const trimmed = folderName.replace(/^\/+|\/+$/g, '');
    if (trimmed) {
      setDestination((prev) => (prev ? `${prev}${trimmed}/` : `${trimmed}/`));
    }
    setFolderModalOpen(false);
    setFolderName('');
  };

  return (
    <div className="animate-fade-in" style={{ flex: 1, overflow: 'auto', padding: 'clamp(16px, 3vw, 32px)', background: BG_BASE }}>
      {/* Breadcrumb */}
      <nav aria-label="Upload breadcrumb">
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 24 }}>
          <Text style={{ color: TEXT_MUTED, fontSize: 13, fontFamily: "var(--font-ui)" }}>Bucket</Text>
          <Text style={{ color: TEXT_MUTED, fontSize: 13 }} aria-hidden="true">&middot;</Text>
          <Text style={{ color: TEXT_SECONDARY, fontSize: 13, fontFamily: "var(--font-mono)" }}>{bucket}</Text>
          <Text style={{ color: TEXT_MUTED, fontSize: 13 }} aria-hidden="true">&middot;</Text>
          <Text style={{ color: ACCENT_BLUE, fontSize: 13, fontWeight: 600, fontFamily: "var(--font-ui)" }} aria-current="page">Upload</Text>
        </div>
      </nav>

      {/* Title */}
      <Title level={1} style={{ color: TEXT_PRIMARY, margin: '0 0 4px', fontWeight: 700, fontSize: 'clamp(20px, 3vw, 24px)', fontFamily: "var(--font-ui)" }}>
        Upload to {bucket}
      </Title>
      <Text style={{ color: TEXT_SECONDARY, fontSize: 13, display: 'block', marginBottom: 24, fontFamily: "var(--font-ui)" }}>
        Drag and drop files, or select files and folders to upload. Files are automatically compressed with delta encoding.
      </Text>

      {/* Upload destination */}
      <div style={{ marginBottom: 24 }}>
        <label htmlFor="upload-destination" style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 1, display: 'block', marginBottom: 8, fontFamily: "var(--font-ui)" }}>
          Upload Destination
        </label>
        <div style={{ display: 'flex', gap: 8 }}>
          <Input
            id="upload-destination"
            value={destination || '/'}
            onChange={(e) => setDestination(e.target.value)}
            style={{ background: 'var(--input-bg)', borderColor: BORDER, color: TEXT_PRIMARY, fontFamily: "var(--font-mono)", fontSize: 13, flex: 1, borderRadius: 8 }}
          />
          <Button
            icon={<FolderAddOutlined />}
            onClick={handleNewFolder}
            style={{ background: BG_ELEVATED, borderColor: BORDER, color: TEXT_SECONDARY, borderRadius: 8 }}
          >
            New folder
          </Button>
        </div>
      </div>

      {/* New folder modal */}
      <Modal
        title="Create folder"
        open={folderModalOpen}
        onOk={handleFolderConfirm}
        onCancel={() => setFolderModalOpen(false)}
        okText="Create"
      >
        <label htmlFor="new-folder-name" style={{ display: 'block', marginBottom: 8, fontSize: 13, color: TEXT_PRIMARY, fontFamily: "var(--font-ui)" }}>
          Folder name
        </label>
        <Input
          id="new-folder-name"
          value={folderName}
          onChange={(e) => setFolderName(e.target.value)}
          onPressEnter={handleFolderConfirm}
          placeholder="my-folder"
          autoFocus
          style={{ fontFamily: "var(--font-mono)" }}
        />
      </Modal>

      {/* Session statistics */}
      <div style={{ marginBottom: 24 }}>
        <Text style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 1, display: 'block', marginBottom: 12, fontFamily: "var(--font-ui)" }}>
          Upload Session Statistics
        </Text>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))', gap: 12 }} role="status" aria-live="polite">
          {[
            { label: 'Files uploaded', value: String(stats.uploaded), color: ACCENT_BLUE },
            { label: 'Original size', value: formatBytes(stats.originalSize), color: ACCENT_PURPLE },
            { label: 'Stored', value: formatBytes(stats.storedSize), color: ACCENT_GREEN },
            { label: 'Space saved', value: `${savings.toFixed(1)}%`, color: savings > 0 ? ACCENT_GREEN : TEXT_MUTED },
          ].map((stat) => (
            <div
              key={stat.label}
              className="glass-card"
              style={{
                borderRadius: 10,
                padding: '14px 16px',
              }}
            >
              <Text style={{ fontSize: 11, color: TEXT_MUTED, display: 'block', marginBottom: 4, fontFamily: "var(--font-ui)" }}>{stat.label}</Text>
              <Text style={{ fontSize: 20, fontWeight: 700, color: stat.color, fontFamily: "var(--font-mono)" }}>{stat.value}</Text>
            </div>
          ))}
        </div>
      </div>

      {/* Drop zone */}
      <div
        ref={dropRef}
        tabIndex={0}
        role="button"
        aria-label="Drop files here to upload, or press Enter to select files"
        onKeyDown={(e) => {
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            fileInputRef.current?.click();
          }
        }}
        style={{
          border: `2px dashed ${dragging ? ACCENT_BLUE : BORDER}`,
          borderRadius: 14,
          padding: 'clamp(32px, 5vw, 48px) 24px',
          textAlign: 'center',
          marginBottom: 24,
          background: dragging ? 'var(--drop-glow)' : 'var(--glass-bg)',
          transition: 'all 0.25s ease',
          cursor: 'pointer',
          animation: dragging ? 'dropGlow 2s ease-in-out infinite' : undefined,
        }}
        onClick={() => fileInputRef.current?.click()}
      >
        <CloudUploadOutlined aria-hidden="true" style={{ fontSize: 48, color: dragging ? ACCENT_BLUE : TEXT_MUTED, marginBottom: 16, transition: 'color 0.2s' }} />
        <div style={{ marginBottom: 12 }}>
          <Text style={{ fontSize: 15, color: TEXT_PRIMARY, fontFamily: "var(--font-ui)", fontWeight: 500 }}>Drag and drop files here</Text>
        </div>
        <div style={{ marginBottom: 20 }}>
          <Text style={{ fontSize: 12, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>or use the buttons below to select files</Text>
        </div>
        <Space size={12} onClick={(e) => e.stopPropagation()}>
          <Button
            type="primary"
            icon={<CloudUploadOutlined />}
            onClick={() => fileInputRef.current?.click()}
            style={{ borderRadius: 8 }}
          >
            Select files
          </Button>
          <Button
            icon={<FolderAddOutlined />}
            onClick={() => folderInputRef.current?.click()}
            style={{ background: BG_ELEVATED, borderColor: BORDER, color: TEXT_SECONDARY, borderRadius: 8 }}
          >
            Select folder
          </Button>
        </Space>
      </div>

      {/* Hidden file inputs */}
      <input
        ref={fileInputRef}
        type="file"
        multiple
        style={{ display: 'none' }}
        aria-hidden="true"
        onChange={(e) => {
          if (e.target.files?.length) addFiles(e.target.files);
          e.target.value = '';
        }}
      />
      <input
        ref={folderInputRef}
        type="file"
        multiple
        {...({ webkitdirectory: '', directory: '' } as React.InputHTMLAttributes<HTMLInputElement>)}
        style={{ display: 'none' }}
        aria-hidden="true"
        onChange={(e) => {
          if (e.target.files?.length) addFiles(e.target.files);
          e.target.value = '';
        }}
      />

      {/* Upload queue */}
      {queue.length > 0 && (
        <div style={{ marginBottom: 24 }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 12 }}>
            <Text style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 1, fontFamily: "var(--font-ui)" }}>
              Upload Queue ({queue.length})
            </Text>
            <Button
              type="text"
              size="small"
              icon={<DeleteOutlined />}
              onClick={clearCompleted}
              style={{ color: TEXT_MUTED, fontSize: 12 }}
            >
              Clear completed
            </Button>
          </div>

          <div
            role="list"
            aria-label="Upload queue"
            className="glass-card"
            style={{
              borderRadius: 10,
              overflow: 'hidden',
            }}
          >
            {queue.map((item) => (
              <div
                key={item.id}
                role="listitem"
                aria-label={`${item.file.name} — ${item.status}`}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: 12,
                  padding: '10px 16px',
                  borderBottom: `1px solid ${BORDER}`,
                }}
              >
                {item.status === 'done' && <CheckCircleFilled aria-label="Upload complete" style={{ color: ACCENT_GREEN, fontSize: 16 }} />}
                {item.status === 'error' && <CloseCircleFilled aria-label="Upload failed" style={{ color: ACCENT_RED, fontSize: 16 }} />}
                {item.status === 'uploading' && <LoadingOutlined aria-label="Uploading" style={{ color: ACCENT_BLUE, fontSize: 16 }} />}
                {item.status === 'pending' && <div role="img" aria-label="Pending" style={{ width: 16, height: 16, borderRadius: '50%', border: `2px solid ${BORDER}` }} />}

                <Text
                  style={{
                    flex: 1,
                    fontFamily: "var(--font-mono)",
                    fontSize: 13,
                    color: item.status === 'error' ? ACCENT_RED : TEXT_PRIMARY,
                  }}
                  ellipsis
                >
                  {item.file.name}
                </Text>

                {item.error && (
                  <Text role="alert" style={{ fontSize: 11, color: ACCENT_RED }}>{item.error}</Text>
                )}

                <Text style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: TEXT_MUTED, whiteSpace: 'nowrap' }}>
                  {formatBytes(item.originalSize)}
                </Text>

                {item.status === 'uploading' && (
                  <div
                    aria-label="Uploading"
                    style={{
                      width: 48,
                      height: 4,
                      borderRadius: 2,
                      background: `${ACCENT_BLUE}22`,
                      overflow: 'hidden',
                      flexShrink: 0,
                    }}
                  >
                    <div style={{
                      width: '40%',
                      height: '100%',
                      borderRadius: 2,
                      background: ACCENT_BLUE,
                      animation: 'shimmer 1.2s ease-in-out infinite',
                      backgroundSize: '200% 100%',
                      backgroundImage: `linear-gradient(90deg, ${ACCENT_BLUE}, ${ACCENT_BLUE}66, ${ACCENT_BLUE})`,
                    }} />
                  </div>
                )}
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Back button */}
      <Button
        icon={<ArrowLeftOutlined />}
        onClick={handleBack}
        style={{ background: BG_ELEVATED, borderColor: BORDER, color: TEXT_SECONDARY, borderRadius: 8 }}
      >
        Back to browse
      </Button>

      {pendingCount > 0 && (
        <Text aria-live="polite" role="status" style={{ marginLeft: 16, fontSize: 12, color: ACCENT_BLUE, fontFamily: "var(--font-mono)" }}>
          {pendingCount} file{pendingCount !== 1 ? 's' : ''} remaining...
        </Text>
      )}
    </div>
  );
}
