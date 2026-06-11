import { useState, useRef, useEffect } from 'react';
import { Button, Typography, Input, Space, Modal } from 'antd';
import {
  CloudUploadOutlined,
  FolderAddOutlined,
  ArrowLeftOutlined,
  DeleteOutlined,
  CheckCircleFilled,
} from '@ant-design/icons';
import { getBucket } from '../s3client';
import { formatBytes } from '../utils';
import useUploadQueue from '../useUploadQueue';
import { useColors } from '../ThemeContext';
import UploadProgressList from './UploadProgressList';

const { Text, Title } = Typography;

interface Props {
  prefix: string;
  onBack: () => void;
  onDone: () => void;
  /** Files dropped onto the browser, staged for confirmation before upload. */
  initialFiles?: File[];
  /** Called once after the staged files have been taken, so App can clear them. */
  onConsumeInitialFiles?: () => void;
  /** "Big OK" when uploads finish: jump to the browser at the destination prefix. */
  onFinish?: (destinationPrefix: string) => void;
}

/** Normalize a destination prefix: strip leading/trailing/duplicate slashes. */
function normalizeDestination(dest: string): string {
  return dest.replace(/^\/+/, '').replace(/\/+$/, '').replace(/\/{2,}/g, '/');
}

export default function UploadPage({ prefix, onBack, onDone, initialFiles, onConsumeInitialFiles, onFinish }: Props) {
  const {
    BG_BASE, BG_ELEVATED, BORDER, TEXT_PRIMARY,
    TEXT_SECONDARY, TEXT_MUTED, ACCENT_BLUE, ACCENT_GREEN, ACCENT_RED, ACCENT_PURPLE, ACCENT_AMBER,
  } = useColors();
  const [destination, setDestination] = useState(prefix);
  const [dragging, setDragging] = useState(false);
  const [folderModalOpen, setFolderModalOpen] = useState(false);
  const [folderName, setFolderName] = useState('');
  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);
  const dropRef = useRef<HTMLDivElement>(null);

  const bucket = getBucket();
  // Files staged from a drop (Finder→browser OR onto this page's drop zone),
  // awaiting the user's confirmation of the destination before they enter the
  // upload queue. Only the explicit "Select files/folder" buttons commit
  // immediately — a deliberate pick made while looking at the destination.
  const [pendingFiles, setPendingFiles] = useState<File[]>([]);
  const {
    queue,
    stats,
    savings,
    pendingCount,
    activeCount,
    addFiles,
    clearCompleted,
    cancelUpload,
    retryUpload,
  } = useUploadQueue(destination);

  // Take the dropped files once, into the pending-confirmation staging area.
  const consumedRef = useRef(false);
  useEffect(() => {
    if (consumedRef.current) return;
    if (initialFiles && initialFiles.length > 0) {
      consumedRef.current = true;
      setPendingFiles(initialFiles);
      onConsumeInitialFiles?.();
    }
  }, [initialFiles, onConsumeInitialFiles]);

  const commitPending = () => {
    if (pendingFiles.length === 0) return;
    addFiles(pendingFiles);
    setPendingFiles([]);
  };

  const normalizedDest = normalizeDestination(destination);
  const destLabel = normalizedDest ? `${normalizedDest}/` : '/ (bucket root)';

  // Finished = the queue has settled (nothing queued/uploading) AND at least one
  // file actually succeeded. A batch that ALL failed leaves pendingCount at 0
  // too, but must NOT show the green "Done — go to folder" affordance — the user
  // should retry, not be sent to an empty folder.
  const succeeded = queue.filter((i) => i.status === 'success');
  const allUploaded = queue.length > 0 && pendingCount === 0 && succeeded.length > 0;
  // Navigate to where the files ACTUALLY landed (the destination captured on the
  // queued items at upload time), not the live input — which the user may have
  // edited after the upload finished. Use the LAST success so a session with two
  // batches to different folders lands at the most recent one.
  const uploadedDest = succeeded[succeeded.length - 1]?.destination ?? normalizedDest;
  const uploadedDestLabel = uploadedDest ? `${uploadedDest}/` : '/ (bucket root)';

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
      dragCount = Math.max(0, dragCount - 1);
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
        // Stage dropped files (don't upload yet) so the destination can be
        // confirmed — consistent with a Finder→browser drop. The explicit
        // "Select files/folder" buttons commit immediately instead.
        const dropped = Array.from(e.dataTransfer.files);
        setPendingFiles((prev) => [...prev, ...dropped]);
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
    // setPendingFiles is stable; the listeners need no other deps.
  }, []);

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
      setDestination((prev) => {
        const base = prev ? prev.replace(/\/+$/, '') : '';
        return base ? `${base}/${trimmed}/` : `${trimmed}/`;
      });
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

      {/* Upload destination — the clear, editable target path. */}
      <div
        style={{
          marginBottom: 24,
          padding: 16,
          borderRadius: 12,
          background: BG_ELEVATED,
          border: `1px solid ${BORDER}`,
        }}
      >
        <label htmlFor="upload-destination" style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 1, display: 'block', marginBottom: 8, fontFamily: "var(--font-ui)" }}>
          Files will be uploaded to
        </label>
        {/* Full target path readout: bucket / prefix — the unambiguous landing spot. */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 10, flexWrap: 'wrap', fontFamily: 'var(--font-mono)', fontSize: 14 }}>
          <CloudUploadOutlined aria-hidden="true" style={{ color: ACCENT_BLUE }} />
          <Text style={{ color: TEXT_SECONDARY, fontFamily: 'var(--font-mono)' }}>{bucket}</Text>
          <Text style={{ color: TEXT_MUTED }} aria-hidden="true">/</Text>
          <Text strong style={{ color: ACCENT_BLUE, fontFamily: 'var(--font-mono)' }} aria-live="polite">
            {normalizedDest ? `${normalizedDest}/` : '(bucket root)'}
          </Text>
        </div>
        <div style={{ display: 'flex', gap: 8 }}>
          <Input
            id="upload-destination"
            value={destination}
            onChange={(e) => setDestination(e.target.value)}
            placeholder="/ (bucket root)"
            aria-label="Destination path prefix — edit to change where files land"
            style={{ background: 'var(--input-bg)', borderColor: BORDER, color: TEXT_PRIMARY, fontFamily: "var(--font-mono)", fontSize: 13, flex: 1, borderRadius: 8 }}
          />
          <Button
            icon={<FolderAddOutlined />}
            onClick={handleNewFolder}
            style={{ background: BG_BASE, borderColor: BORDER, color: TEXT_SECONDARY, borderRadius: 8 }}
          >
            New folder
          </Button>
        </div>
        <Text style={{ display: 'block', marginTop: 8, fontSize: 12, color: TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
          Edit the path above to change the destination folder. It is created if it doesn&rsquo;t exist.
        </Text>
      </div>

      {/* Pending dropped files — confirm the destination before they upload. */}
      {pendingFiles.length > 0 && (
        <div
          style={{
            marginBottom: 24,
            padding: 16,
            borderRadius: 12,
            background: 'var(--drop-glow)',
            border: `1px solid ${ACCENT_BLUE}`,
          }}
        >
          <Text style={{ display: 'block', fontSize: 14, color: TEXT_PRIMARY, fontFamily: 'var(--font-ui)', fontWeight: 600, marginBottom: 4 }}>
            {pendingFiles.length} file{pendingFiles.length !== 1 ? 's' : ''} ready to upload
          </Text>
          <Text style={{ display: 'block', fontSize: 12, color: TEXT_SECONDARY, fontFamily: 'var(--font-ui)', marginBottom: 12 }}>
            Check the destination above, then start the upload. {pendingFiles.slice(0, 3).map((f) => f.name).join(', ')}
            {pendingFiles.length > 3 ? `, +${pendingFiles.length - 3} more` : ''}
          </Text>
          <Space>
            <Button
              type="primary"
              size="large"
              icon={<CloudUploadOutlined />}
              onClick={commitPending}
              style={{ borderRadius: 8, fontWeight: 600 }}
            >
              Upload {pendingFiles.length} file{pendingFiles.length !== 1 ? 's' : ''} to {destLabel}
            </Button>
            <Button
              size="large"
              onClick={() => setPendingFiles([])}
              style={{ background: BG_ELEVATED, borderColor: BORDER, color: TEXT_SECONDARY, borderRadius: 8 }}
            >
              Cancel
            </Button>
          </Space>
        </div>
      )}

      {/* New folder modal */}
      <Modal
        title="Create folder"
        open={folderModalOpen}
        onOk={handleFolderConfirm}
        onCancel={() => {
          setFolderModalOpen(false);
          setFolderName('');
        }}
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
            { label: 'Active uploads', value: String(activeCount), color: activeCount > 0 ? ACCENT_BLUE : TEXT_MUTED },
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

          <UploadProgressList
            queue={queue}
            borderColor={BORDER}
            textPrimary={TEXT_PRIMARY}
            textMuted={TEXT_MUTED}
            accentBlue={ACCENT_BLUE}
            accentGreen={ACCENT_GREEN}
            accentRed={ACCENT_RED}
            finalizingColor={ACCENT_AMBER}
            onCancelUpload={cancelUpload}
            onRetryUpload={retryUpload}
          />
        </div>
      )}

      {/* Footer actions. When every upload has finished, lead with the big
          "Done" button that jumps straight to the folder the files landed in. */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap' }}>
        {allUploaded ? (
          <>
            <Button
              type="primary"
              size="large"
              icon={<CheckCircleFilled />}
              onClick={() => {
                onDone();
                if (onFinish) onFinish(uploadedDest);
                else onBack();
              }}
              style={{ borderRadius: 8, fontWeight: 600, minWidth: 220 }}
            >
              Done — go to {uploadedDestLabel}
            </Button>
            <Text aria-live="polite" role="status" style={{ fontSize: 13, color: ACCENT_GREEN, fontFamily: 'var(--font-ui)' }}>
              {stats.uploaded} file{stats.uploaded !== 1 ? 's' : ''} uploaded
            </Text>
          </>
        ) : (
          <>
            <Button
              icon={<ArrowLeftOutlined />}
              onClick={handleBack}
              style={{ background: BG_ELEVATED, borderColor: BORDER, color: TEXT_SECONDARY, borderRadius: 8 }}
            >
              Back to browse
            </Button>
            {pendingCount > 0 && (
              <Text aria-live="polite" role="status" style={{ fontSize: 12, color: ACCENT_BLUE, fontFamily: "var(--font-mono)" }}>
                {pendingCount} file{pendingCount !== 1 ? 's' : ''} remaining...
              </Text>
            )}
          </>
        )}
      </div>
    </div>
  );
}
