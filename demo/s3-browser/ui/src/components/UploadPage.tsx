import { useState, useRef, useEffect } from 'react';
import { AutoComplete, Button, Typography, Input, Space, Modal } from 'antd';
import {
  CloudUploadOutlined,
  FolderAddOutlined,
  ArrowLeftOutlined,
  DeleteOutlined,
  CheckCircleFilled,
  CloseOutlined,
  PlusOutlined,
} from '@ant-design/icons';
import { getBucket } from '../s3client';
import { formatBytes } from '../utils';
import { collectDroppedFiles } from '../droppedFiles';
import { useFolderSuggestions } from '../useFolderSuggestions';
import useUploadQueue from '../useUploadQueue';
import { useColors } from '../ThemeContext';
import UploadProgressList from './UploadProgressList';
import { keyPathError } from './destPrefix';
import { precheckUpload } from '../uploadPrecheck';
import { useUploadLimits } from '../queries/uploadLimits';

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
  /** Can this user write under the prefix (`''` = bucket root)? Per prefix, not per bucket. */
  canWrite?: (prefix: string) => boolean;
  /** Folder roots this user can write to, offered when the destination is read-only. */
  writablePrefixes?: string[];
  /** Admin session: read max_object_size and the bucket quota to refuse files before upload. */
  checkLimits?: boolean;
}

/** Normalize a destination prefix: strip leading/trailing/duplicate slashes. */
function normalizeDestination(dest: string): string {
  return dest.replace(/^\/+/, '').replace(/\/+$/, '').replace(/\/{2,}/g, '/');
}

const ALWAYS = () => true;
const NO_PREFIXES: string[] = [];

/** The staged list shows this many rows; the rest are counted. */
const STAGED_ROWS_SHOWN = 200;

/** "Space saved" for a session that saved nothing. */
const NOTHING_SAVED = 'None';

/** The saving as the stats card shows it: a 0.0 % in a percent box read like a fault. */
function savedText(pct: number): string {
  const text = `${pct.toFixed(1)}%`;
  return text === '0.0%' ? NOTHING_SAVED : text;
}

/** The path a file gets under the destination (a folder pick keeps its tree). */
function stagedPath(file: File): string {
  return (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name;
}

/** One staged file, however often it is picked or dropped. */
function stagedId(file: File): string {
  return `${stagedPath(file)}\u0000${file.size}\u0000${file.lastModified}`;
}

/** `prev` plus the files of `added` that are not in it yet. */
function mergeStaged(prev: File[], added: File[]): File[] {
  const seen = new Set(prev.map(stagedId));
  const next = [...prev];
  for (const f of added) {
    const id = stagedId(f);
    if (seen.has(id)) continue;
    seen.add(id);
    next.push(f);
  }
  return next;
}

export default function UploadPage({
  prefix, onBack, onDone, initialFiles, onConsumeInitialFiles, onFinish,
  canWrite = ALWAYS, writablePrefixes = NO_PREFIXES, checkLimits = false,
}: Props) {
  const {
    BG_BASE, BG_ELEVATED, BORDER, TEXT_PRIMARY,
    TEXT_SECONDARY, TEXT_MUTED, ACCENT_BLUE, ACCENT_GREEN, ACCENT_RED, ACCENT_AMBER,
  } = useColors();
  const [destination, setDestination] = useState(prefix);
  const [dragging, setDragging] = useState(false);
  const [folderModalOpen, setFolderModalOpen] = useState(false);
  const [folderName, setFolderName] = useState('');
  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);
  const pageDropRef = useRef<HTMLDivElement>(null);

  const bucket = getBucket();
  // Every file the user adds (a drop onto the browser or this page, or the
  // "Select files/folder" pickers) is staged here first. Nothing uploads
  // until the user sets the folder, reviews the list and presses Upload.
  const [pendingFiles, setPendingFiles] = useState<File[]>([]);
  const {
    queue,
    stats,
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

  const normalizedDest = normalizeDestination(destination);
  // The destination is never moved behind the user's back: a read-only one
  // blocks the upload and the page offers the writable prefixes instead.
  const pathError = keyPathError(normalizedDest);
  const destReadOnly = !pathError && !canWrite(normalizedDest ? `${normalizedDest}/` : '');
  const destError = pathError ?? (destReadOnly ? 'read-only' : null);
  const folderNameError = keyPathError(folderName.replace(/^\/+|\/+$/g, ''));

  // A bad destination blocks the upload here, at the one place files enter
  // the queue; the message is shown under the destination input.
  // Files the server would refuse (size, quota) never enter the queue: the
  // page says why at once instead of after the bytes went out.
  const limits = useUploadLimits(bucket, checkLimits);
  const [refused, setRefused] = useState<{ file: File; reason: string }[]>([]);
  const stageFiles = (files: FileList | File[]) => {
    const added = Array.from(files);
    if (added.length > 0) setPendingFiles((prev) => mergeStaged(prev, added));
  };
  const unstage = (file: File) => setPendingFiles((prev) => prev.filter((f) => f !== file));

  const pendingCheck = precheckUpload(pendingFiles, limits);
  const pendingBytes = pendingFiles.reduce((sum, f) => sum + f.size, 0);
  const commitPending = () => {
    if (pendingCheck.accepted.length === 0 || destError) return;
    addFiles(pendingCheck.accepted);
    setRefused(pendingCheck.refused);
    setPendingFiles([]);
  };

  // The folder (the readout above shows the bucket), or words for the root
  // instead of a "/ (bucket root)" that read like a path.
  const destLabel = normalizedDest ? `${normalizedDest}/` : 'the bucket root';

  // Existing-folder suggestions for the destination autocomplete (debounced,
  // cached per level; free text still allowed for brand-new folders).
  const folderSuggestions = useFolderSuggestions(bucket, destination);

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
  const uploadedDestLabel = `${bucket}/${uploadedDest ? `${uploadedDest}/` : ''}`;

  // Drag-drop listeners live on the PAGE ROOT, not the dashed drop-zone box:
  // the box is hidden once files are in context (staged or uploading), but
  // dropping more files anywhere on the page must keep working.
  useEffect(() => {
    const el = pageDropRef.current;
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
      if (!e.dataTransfer) return;
      // Stage dropped files (don't upload yet) so the destination can be
      // confirmed, the same as a pick. Dropped FOLDERS are walked into their
      // real files (sync call required, see droppedFiles.ts).
      collectDroppedFiles(e.dataTransfer).then((dropped) => {
        if (dropped.length > 0) setPendingFiles((prev) => mergeStaged(prev, dropped));
      });
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
    if (folderNameError) return;
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
    <div ref={pageDropRef} className="animate-fade-in" style={{ flex: 1, overflow: 'auto', padding: 'clamp(16px, 3vw, 32px)', background: BG_BASE }}>
      {/* No breadcrumb of its own: the header shows the bucket, and the
          sidebar marks "Upload files" as the current page. */}
      {/* Title */}
      <Title level={1} style={{ color: TEXT_PRIMARY, margin: '0 0 4px', fontWeight: 700, fontSize: 'clamp(20px, 3vw, 24px)', fontFamily: "var(--font-ui)" }}>
        Upload to {bucket}
      </Title>
      <Text style={{ color: TEXT_SECONDARY, fontSize: 13, display: 'block', marginBottom: 24, fontFamily: "var(--font-ui)" }}>
        Add files or folders, set the destination folder, then press Upload. Archives, disk images, binaries and model files are stored as deltas when a new version is close to an older one; other files are stored whole.
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
          {normalizedDest ? (
            <Text strong style={{ color: ACCENT_BLUE, fontFamily: 'var(--font-mono)' }} aria-live="polite">
              {`${normalizedDest}/`}
            </Text>
          ) : (
            <Text style={{ color: TEXT_MUTED, fontFamily: 'var(--font-ui)', fontSize: 13 }} aria-live="polite">
              bucket root
            </Text>
          )}
        </div>
        <div style={{ display: 'flex', gap: 8 }}>
          <AutoComplete
            id="upload-destination"
            value={destination}
            onChange={(v) => setDestination(v)}
            options={folderSuggestions.map((f) => ({
              value: f,
              label: <span style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}>{f}</span>,
            }))}
            placeholder="folder/ (optional)"
            aria-label="Destination path prefix — type to autocomplete existing folders"
            status={destError ? 'error' : undefined}
            style={{ flex: 1, fontFamily: 'var(--font-mono)', fontSize: 13 }}
          />
          <Button
            icon={<FolderAddOutlined />}
            onClick={handleNewFolder}
            style={{ background: BG_BASE, borderColor: BORDER, color: TEXT_SECONDARY, borderRadius: 8 }}
          >
            New folder
          </Button>
        </div>
        {pathError && (
          <Text role="alert" style={{ display: 'block', marginTop: 8, fontSize: 12, color: ACCENT_RED, fontFamily: 'var(--font-ui)' }}>
            {pathError}
          </Text>
        )}
        {destReadOnly && (
          <div role="alert" style={{ marginTop: 8, fontSize: 12, color: ACCENT_RED, fontFamily: 'var(--font-ui)' }}>
            <div>
              You cannot upload to {bucket}/{normalizedDest ? `${normalizedDest}/` : ''}: your account has no write
              permission there.
            </div>
            {writablePrefixes.length > 0 && (
              <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 6, marginTop: 6, color: TEXT_SECONDARY }}>
                <span>You can upload to:</span>
                {writablePrefixes.map((p) => (
                  <Button key={p} size="small" onClick={() => setDestination(p)} style={{ fontFamily: 'var(--font-mono)' }}>
                    {p || 'Bucket root'}
                  </Button>
                ))}
              </div>
            )}
          </div>
        )}
        <Text style={{ display: 'block', marginTop: 8, fontSize: 12, color: TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
          Edit the path above to change the destination folder. It is created if it doesn&rsquo;t exist.
        </Text>
      </div>

      {/* Files refused before upload, and the staged files that would be. */}
      {(pendingFiles.length > 0 ? pendingCheck.refused : refused).length > 0 && (
        <div role="alert" style={{ marginBottom: 16, padding: 12, borderRadius: 10, border: `1px solid ${ACCENT_RED}`, fontFamily: 'var(--font-ui)' }}>
          <Text strong style={{ display: 'block', color: ACCENT_RED, fontSize: 13, marginBottom: 4 }}>
            {pendingFiles.length > 0 ? 'These files cannot be uploaded:' : 'Not uploaded:'}
          </Text>
          <ul style={{ margin: 0, paddingLeft: 18, fontSize: 12, color: TEXT_SECONDARY }}>
            {(pendingFiles.length > 0 ? pendingCheck.refused : refused).map((r) => (
              <li key={`${r.file.name}:${r.file.size}`}>{r.file.name}: {r.reason}</li>
            ))}
          </ul>
        </div>
      )}

      {/* Staged files: nothing uploads until the user presses Upload. */}
      {pendingFiles.length > 0 && (
        <section
          aria-label="Files to upload"
          style={{
            marginBottom: 24,
            padding: 16,
            borderRadius: 12,
            background: 'var(--drop-glow)',
            border: `1px solid ${ACCENT_BLUE}`,
          }}
        >
          <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap', marginBottom: 4 }}>
            <Text style={{ fontSize: 14, color: TEXT_PRIMARY, fontFamily: 'var(--font-ui)', fontWeight: 600 }}>
              {pendingFiles.length} file{pendingFiles.length !== 1 ? 's' : ''} ready to upload · {formatBytes(pendingBytes)}
            </Text>
            <Space size={4} wrap>
              <Button type="text" size="small" icon={<PlusOutlined />} onClick={() => fileInputRef.current?.click()} style={{ color: TEXT_SECONDARY, fontSize: 12 }}>
                Add files
              </Button>
              <Button type="text" size="small" icon={<FolderAddOutlined />} onClick={() => folderInputRef.current?.click()} style={{ color: TEXT_SECONDARY, fontSize: 12 }}>
                Add folder
              </Button>
              <Button type="text" size="small" icon={<DeleteOutlined />} onClick={() => setPendingFiles([])} style={{ color: TEXT_SECONDARY, fontSize: 12 }}>
                Clear list
              </Button>
            </Space>
          </div>
          <Text style={{ display: 'block', fontSize: 12, color: TEXT_SECONDARY, fontFamily: 'var(--font-ui)', marginBottom: 12 }}>
            Nothing is uploaded yet. Set the destination folder above, check the list, then press Upload.
          </Text>
          <ul
            aria-label="Staged files"
            style={{
              listStyle: 'none', margin: '0 0 12px', padding: 0, maxHeight: 240, overflow: 'auto',
              borderRadius: 8, border: `1px solid ${BORDER}`, background: BG_ELEVATED,
            }}
          >
            {pendingFiles.slice(0, STAGED_ROWS_SHOWN).map((f) => {
              const path = stagedPath(f);
              return (
                <li
                  key={stagedId(f)}
                  style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '4px 4px 4px 12px', borderTop: `1px solid ${BORDER}`, marginTop: -1 }}
                >
                  <span title={path} style={{ flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontFamily: 'var(--font-mono)', fontSize: 12, color: TEXT_PRIMARY }}>
                    {path}
                  </span>
                  <span style={{ flexShrink: 0, fontFamily: 'var(--font-mono)', fontSize: 12, color: TEXT_MUTED, fontVariantNumeric: 'tabular-nums' }}>
                    {formatBytes(f.size)}
                  </span>
                  <Button
                    type="text"
                    size="small"
                    icon={<CloseOutlined />}
                    aria-label={`Remove ${path} from the list`}
                    title="Remove from the list"
                    onClick={() => unstage(f)}
                    style={{ color: TEXT_MUTED, flexShrink: 0 }}
                  />
                </li>
              );
            })}
            {pendingFiles.length > STAGED_ROWS_SHOWN && (
              <li style={{ padding: '6px 12px', fontSize: 12, color: TEXT_MUTED, fontFamily: 'var(--font-ui)', borderTop: `1px solid ${BORDER}` }}>
                And {(pendingFiles.length - STAGED_ROWS_SHOWN).toLocaleString()} more files
              </li>
            )}
          </ul>
          <Space wrap>
            <Button
              type="primary"
              size="large"
              icon={<CloudUploadOutlined />}
              onClick={commitPending}
              disabled={destError !== null || pendingCheck.accepted.length === 0}
              style={{ borderRadius: 8, fontWeight: 600 }}
            >
              Upload {pendingCheck.accepted.length} file{pendingCheck.accepted.length !== 1 ? 's' : ''} to {destLabel}
            </Button>
            {destError !== null && (
              <Text style={{ fontSize: 12, color: ACCENT_RED, fontFamily: 'var(--font-ui)' }}>
                Fix the destination folder to upload.
              </Text>
            )}
          </Space>
        </section>
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
        okButtonProps={{ disabled: folderNameError !== null }}
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
          status={folderNameError ? 'error' : undefined}
        />
        {folderNameError && (
          <Text role="alert" style={{ display: 'block', marginTop: 6, fontSize: 12, color: ACCENT_RED }}>
            {folderNameError}
          </Text>
        )}
      </Modal>

      {/* Session statistics — only once an upload has actually started; a wall
          of zeros above the confirm button is noise. */}
      {queue.length > 0 && (
      <div style={{ marginBottom: 24 }}>
        <Text style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 1, display: 'block', marginBottom: 12, fontFamily: "var(--font-ui)" }}>
          Upload session
        </Text>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))', gap: 12 }} role="status" aria-live="polite">
          {[
            // Plain numbers in one colour: only a real saving is green.
            { label: 'Files uploaded', value: String(stats.uploaded), color: TEXT_PRIMARY },
            { label: 'Original size', value: formatBytes(stats.originalSize), color: TEXT_PRIMARY },
            {
              label: 'Stored',
              value: stats.storedSize === null ? '—' : formatBytes(stats.storedSize),
              color: TEXT_PRIMARY,
              hint: stats.storedSize === null
                ? 'Reading the stored size of each uploaded file'
                : stats.baselineCount > 0
                  ? 'Includes the full copy stored as the baseline of each new folder'
                  : undefined,
            },
            {
              label: 'Space saved',
              value: stats.savingsPct === null ? '—' : savedText(stats.savingsPct),
              color: stats.savingsPct !== null && savedText(stats.savingsPct) !== NOTHING_SAVED ? ACCENT_GREEN : TEXT_MUTED,
              hint: stats.savingsPct === null
                ? 'Reading the stored size of each uploaded file'
                : savedText(stats.savingsPct) === NOTHING_SAVED
                  ? 'The files are stored whole: no space is saved yet.'
                  : undefined,
            },
            { label: 'Active uploads', value: String(activeCount), color: activeCount > 0 ? ACCENT_BLUE : TEXT_MUTED },
          ].map((stat) => (
            <div
              key={stat.label}
              title={'hint' in stat ? stat.hint : undefined}
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
        {stats.baselineCount > 0 && stats.storedSize !== null && (
          <Text style={{ display: 'block', marginTop: 10, fontSize: 12, color: TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
            {stats.baselineCount === 1 ? 'One upload was' : `${stats.baselineCount} uploads were`} the first
            file in {stats.baselineCount === 1 ? 'its folder' : 'their folders'}. The first file is stored in
            full as the folder&apos;s baseline, and later versions are stored as small deltas against it. The
            figures above include {stats.baselineCount === 1 ? 'that baseline' : 'those baselines'}, so the
            savings grow as you upload more versions.
          </Text>
        )}
      </div>
      )}

      {/* Drop-zone box — only while there are no files in context yet. Once
          files are staged or uploading it's noise; dropping more files
          anywhere on the page still works (listeners are on the page root). */}
      {pendingFiles.length === 0 && queue.length === 0 && (
      // Not role="button": it holds the two real buttons (axe
      // nested-interactive). A click anywhere is a mouse shortcut to them.
      <div
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
          <Text style={{ fontSize: 12, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>or select files. You check the list before the upload starts.</Text>
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
      )}

      {/* Hidden file inputs */}
      <input
        ref={fileInputRef}
        type="file"
        multiple
        style={{ display: 'none' }}
        aria-hidden="true"
        onChange={(e) => {
          if (e.target.files?.length) stageFiles(e.target.files);
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
          if (e.target.files?.length) stageFiles(e.target.files);
          e.target.value = '';
        }}
      />

      {/* Primary actions — ABOVE the progress list so "Done" is reachable
          without scrolling past every progress bar. */}
      {allUploaded ? (
        /* Upload complete: a centered, dominant, gently celebratory panel —
           the Done button is THE action on this page now. */
        <div
          className="dg-upload-success"
          role="status"
          aria-live="polite"
          style={{
            marginBottom: 24,
            padding: '28px 24px',
            borderRadius: 14,
            border: `1px solid ${ACCENT_GREEN}`,
            background: 'var(--drop-glow)',
            display: 'flex',
            flexDirection: 'column',
            alignItems: 'center',
            gap: 8,
            textAlign: 'center',
          }}
        >
          <CheckCircleFilled aria-hidden="true" style={{ fontSize: 44, color: ACCENT_GREEN }} />
          <Text style={{ fontSize: 18, fontWeight: 700, color: TEXT_PRIMARY, fontFamily: 'var(--font-ui)' }}>
            Upload complete
          </Text>
          <Text style={{ fontSize: 13, color: TEXT_SECONDARY, fontFamily: 'var(--font-ui)' }}>
            {stats.uploaded} file{stats.uploaded !== 1 ? 's' : ''} uploaded to{' '}
            <Text code style={{ fontSize: 13 }}>{uploadedDestLabel}</Text>
          </Text>
          <Button
            type="primary"
            size="large"
            icon={<CheckCircleFilled />}
            onClick={() => {
              onDone();
              if (onFinish) onFinish(uploadedDest);
              else onBack();
            }}
            style={{ borderRadius: 10, fontWeight: 600, minWidth: 280, height: 48, fontSize: 16, marginTop: 8 }}
          >
            {uploadedDest ? 'Done — open folder' : 'Done — back to browser'}
          </Button>
        </div>
      ) : (
        <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap', marginBottom: 24 }}>
          <Button
            icon={<ArrowLeftOutlined />}
            onClick={handleBack}
            style={{ background: BG_ELEVATED, borderColor: BORDER, color: TEXT_SECONDARY, borderRadius: 8 }}
          >
            Back to browser
          </Button>
          {pendingCount > 0 && (
            <Text aria-live="polite" role="status" style={{ fontSize: 12, color: ACCENT_BLUE, fontFamily: "var(--font-mono)" }}>
              {pendingCount} file{pendingCount !== 1 ? 's' : ''} remaining…
            </Text>
          )}
        </div>
      )}

      {/* Upload queue */}
      {queue.length > 0 && (
        <div style={{ marginBottom: 24 }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 12 }}>
            <Text style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 1, fontFamily: "var(--font-ui)" }}>
              Upload queue ({queue.length})
            </Text>
            <Space size={4}>
              {/* The big drop-zone box is hidden once files are in context;
                  this keeps an explicit add-more affordance reachable. */}
              <Button
                type="text"
                size="small"
                icon={<CloudUploadOutlined />}
                onClick={() => fileInputRef.current?.click()}
                style={{ color: TEXT_MUTED, fontSize: 12 }}
              >
                Add files
              </Button>
              <Button
                type="text"
                size="small"
                icon={<DeleteOutlined />}
                onClick={clearCompleted}
                style={{ color: TEXT_MUTED, fontSize: 12 }}
              >
                Clear completed
              </Button>
            </Space>
          </div>

          {/* The list scrolls past ~6 rows and renders only the rows in view. */}
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
    </div>
  );
}
