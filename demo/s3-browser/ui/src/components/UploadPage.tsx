import { useState, useRef, useEffect, type CSSProperties } from 'react';
import { AutoComplete, Button, Typography, Input, Space, Modal } from 'antd';
import {
  CloudUploadOutlined,
  FolderAddOutlined,
  ArrowLeftOutlined,
  DeleteOutlined,
  CheckCircleFilled,
  CloseCircleFilled,
  ExclamationCircleFilled,
  MinusCircleFilled,
  CloseOutlined,
  PlusOutlined,
  ReloadOutlined,
} from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
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
import { qk } from '../queries/keys';
import { savingsColors } from '../savings';
import {
  canRetry,
  mergeStaged,
  outcomeHeadline,
  queueOutcome,
  stagedHeadline,
  stagedId,
  stagedRows,
  uploadButtonLabel,
} from '../uploadOutcome';
import TruncatedName from './TruncatedName';
import { CARD_RADIUS, SPACE } from './shared-styles';

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
/** About this many staged rows fit the list box before it scrolls. */
const STAGED_ROWS_VISIBLE = 8;

/** The header counts and quota refresh this long after the last file settles (D7). */
const USAGE_REFRESH_MS = 400;

/** The staged files, and the paths a later pick or drop replaced (D27). */
interface Staged {
  files: File[];
  replaced: ReadonlySet<string>;
}
const NOTHING_STAGED: Staged = { files: [], replaced: new Set() };

/** Pure: stage `added`. A file already staged is ignored; a file with a
 *  staged path replaces that entry and is marked. */
function withStaged(prev: Staged, added: File[]): Staged {
  if (added.length === 0) return prev;
  const { files, replaced } = mergeStaged(prev.files, added);
  return { files, replaced: replaced.length > 0 ? new Set([...prev.replaced, ...replaced]) : prev.replaced };
}

export default function UploadPage({
  prefix, onBack, onDone, initialFiles, onConsumeInitialFiles, onFinish,
  canWrite = ALWAYS, writablePrefixes = NO_PREFIXES, checkLimits = false,
}: Props) {
  const colors = useColors();
  const {
    BG_BASE, BG_ELEVATED, BORDER, TEXT_PRIMARY,
    TEXT_SECONDARY, TEXT_MUTED, ACCENT_BLUE, ACCENT_GREEN, ACCENT_RED, ACCENT_AMBER,
  } = colors;
  const [destination, setDestination] = useState(prefix);
  const [dragging, setDragging] = useState(false);
  const [folderModalOpen, setFolderModalOpen] = useState(false);
  const [folderName, setFolderName] = useState('');
  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);
  const pageDropRef = useRef<HTMLDivElement>(null);
  const queueHeadingRef = useRef<HTMLDivElement>(null);
  const queryClient = useQueryClient();

  const bucket = getBucket();
  // Every file the user adds (a drop onto the browser or this page, or the
  // "Add files/folder" pickers) is staged here first. Nothing uploads until
  // the user sets the folder, reviews the list and presses Upload.
  // A file whose path is already staged replaces that row, and the row says
  // so, instead of a second row that would overwrite the first (same key).
  const [staged, setStaged] = useState<Staged>(NOTHING_STAGED);
  const pendingFiles = staged.files;
  const replacedPaths = staged.replaced;
  const {
    queue,
    stats,
    addFiles,
    clearCompleted,
    cancelUpload,
    retryUpload,
    removeUpload,
  } = useUploadQueue(destination);

  const stage = (added: File[]) => setStaged((prev) => withStaged(prev, added));

  // Take the dropped files once, into the pending-confirmation staging area.
  const consumedRef = useRef(false);
  useEffect(() => {
    if (consumedRef.current) return;
    if (initialFiles && initialFiles.length > 0) {
      consumedRef.current = true;
      setStaged({ files: initialFiles, replaced: new Set() });
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

  // Files the server would refuse (size, quota) never enter the queue: their
  // row says why at once, instead of after the bytes went out.
  const limits = useUploadLimits(bucket, checkLimits);
  const unstage = (file: File) => setStaged((prev) => ({ ...prev, files: prev.files.filter((f) => f !== file) }));
  const clearStaged = () => setStaged(NOTHING_STAGED);

  // ONE state per file drives the list, the counts, the button and the banner.
  const pendingCheck = precheckUpload(pendingFiles, limits);
  const rows = stagedRows(pendingFiles, pendingCheck.refused);
  const ready = pendingCheck.accepted.length;
  const commitPending = () => {
    if (ready === 0 || destError) return;
    addFiles(pendingCheck.accepted);
    // Refused files stay staged, with their reason: the user removes them
    // or frees space; they never vanish into a separate "not uploaded" box.
    setStaged({ files: pendingCheck.refused.map((r) => r.file), replaced: new Set() });
    // The Upload button unmounts: keep the keyboard focus on the page.
    window.setTimeout(() => queueHeadingRef.current?.focus(), 0);
  };

  // The folder (the readout above shows the bucket), or words for the root
  // instead of a "/ (bucket root)" that read like a path.
  const destLabel = normalizedDest ? `${normalizedDest}/` : 'the bucket root';

  // Existing-folder suggestions for the destination autocomplete (debounced,
  // cached per level; free text still allowed for brand-new folders).
  const folderSuggestions = useFolderSuggestions(bucket, destination);

  const outcome = queueOutcome(queue);
  const settled = outcome.done + outcome.failed + outcome.cancelled;
  // Navigate to where the files ACTUALLY landed (the destination captured on the
  // queued items at upload time), not the live input — which the user may have
  // edited after the upload finished. Use the LAST success so a session with two
  // batches to different folders lands at the most recent one.
  const succeeded = queue.filter((i) => i.status === 'success');
  const uploadedDest = succeeded[succeeded.length - 1]?.destination ?? normalizedDest;
  const uploadedDestLabel = `${bucket}/${uploadedDest ? `${uploadedDest}/` : ''}`;
  const headline = outcomeHeadline(outcome, uploadedDestLabel);

  // The header's object count, size and quota chip (and the quota pre-check)
  // read the bucket usage: refresh it when files land or fail (a quota
  // refusal means the usage changed too), not only on the next page.
  const uploadBuckets = [...new Set(queue.map((i) => i.bucket))].join('\u0000');
  useEffect(() => {
    if (settled === 0) return;
    const timer = window.setTimeout(() => {
      for (const b of uploadBuckets.split('\u0000')) {
        if (b) void queryClient.invalidateQueries({ queryKey: qk.bucketUsage(b) });
      }
    }, USAGE_REFRESH_MS);
    return () => window.clearTimeout(timer);
  }, [settled, uploadBuckets, queryClient]);

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
      collectDroppedFiles(e.dataTransfer).then((dropped) => setStaged((prev) => withStaged(prev, dropped)));
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
  }, []);

  // The staged list scrolls; a fade at its foot says there is more until
  // the user reaches the end (D28).
  const [stagedAtEnd, setStagedAtEnd] = useState(true);
  const stagedListRef = useRef<HTMLUListElement>(null);
  const shownRows = Math.min(rows.length, STAGED_ROWS_SHOWN);
  useEffect(() => {
    const el = stagedListRef.current;
    setStagedAtEnd(!el || el.scrollHeight - el.scrollTop - el.clientHeight < 2 || shownRows <= STAGED_ROWS_VISIBLE);
  }, [shownRows]);

  const handleBack = () => {
    onDone();
    onBack();
  };

  const finish = () => {
    onDone();
    if (onFinish) onFinish(uploadedDest);
    else onBack();
  };

  const retryFailed = () => {
    for (const item of queue) if (item.status === 'error' && canRetry(item)) retryUpload(item.id);
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

  const hasFiles = pendingFiles.length > 0 || queue.length > 0;
  const card: CSSProperties = {
    marginBottom: SPACE.lg,
    padding: SPACE.md,
    borderRadius: CARD_RADIUS,
    background: BG_ELEVATED,
    border: `1px solid ${BORDER}`,
  };
  const ui = (size: number, color: string): CSSProperties => ({ fontSize: size, color, fontFamily: 'var(--font-ui)' });

  // The final banner: green ONLY when every file succeeded (D23).
  const tone = outcome.tone;
  const toneColor = tone === 'success' ? ACCENT_GREEN : tone === 'partial' ? ACCENT_AMBER : tone === 'failed' ? ACCENT_RED : TEXT_MUTED;
  const ToneIcon = tone === 'success' ? CheckCircleFilled : tone === 'partial' ? ExclamationCircleFilled : tone === 'failed' ? CloseCircleFilled : MinusCircleFilled;
  const showBanner = tone === 'success' || tone === 'partial' || tone === 'failed' || tone === 'cancelled';

  // Session figures: only real numbers; no "None", no zero tiles (D8).
  const saved = stats.savingsPct !== null && stats.savingsPct > 0 ? stats.savingsPct : null;
  const sessionTiles: { label: string; value: string; color?: string; hint?: string }[] = [
    { label: 'Files uploaded', value: stats.uploaded.toLocaleString() },
    { label: 'Original size', value: formatBytes(stats.originalSize) },
    {
      label: 'Stored',
      value: stats.storedSize === null ? '…' : formatBytes(stats.storedSize),
      hint: stats.storedSize === null
        ? 'Reading the stored size of each uploaded file'
        : stats.baselineCount > 0
          ? 'Includes the full copy stored as the baseline of each new folder'
          : undefined,
    },
    ...(saved !== null ? [{ label: 'Space saved', value: `${saved.toFixed(1)}%`, color: savingsColors(colors).text }] : []),
  ];

  return (
    <div ref={pageDropRef} style={{ flex: 1, minHeight: 0, position: 'relative', display: 'flex', flexDirection: 'column' }}>
      <div className="animate-fade-in" style={{ flex: 1, overflow: 'auto', padding: `clamp(${SPACE.md}px, 3vw, ${SPACE.xl}px)`, background: BG_BASE }}>
        {/* No breadcrumb of its own: the header shows the bucket, and the
            sidebar marks "Upload files" as the current page. */}
        <Title level={1} style={{ color: TEXT_PRIMARY, margin: `0 0 ${SPACE.xxs}px`, fontWeight: 700, fontSize: 'clamp(20px, 3vw, 24px)', fontFamily: 'var(--font-ui)' }}>
          Upload to {bucket}
        </Title>
        <Text style={{ ...ui(13, TEXT_SECONDARY), display: 'block', marginBottom: SPACE.lg }}>
          Add files or folders, set the destination folder, then press Upload. Archives, disk images, binaries and model files are stored as deltas when a new version is close to an older one; other files are stored whole.
        </Text>

        {/* Upload destination — the clear, editable target path. */}
        <div style={card}>
          <label htmlFor="upload-destination" style={{ ...ui(14, TEXT_PRIMARY), fontWeight: 600, display: 'block', marginBottom: SPACE.xs }}>
            Files will be uploaded to
          </label>
          {/* Full target path readout: bucket / prefix — the landing spot. An
              invalid path is shown in the error colour, never as accepted (D29). */}
          <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.xxs, marginBottom: SPACE.sm, flexWrap: 'wrap', fontFamily: 'var(--font-mono)', fontSize: 14 }}>
            <CloudUploadOutlined aria-hidden="true" style={{ color: destError ? ACCENT_RED : ACCENT_BLUE }} />
            <Text style={{ color: TEXT_SECONDARY, fontFamily: 'var(--font-mono)' }}>{bucket}</Text>
            <Text style={{ color: TEXT_MUTED }} aria-hidden="true">/</Text>
            {normalizedDest ? (
              <Text strong style={{ color: destError ? ACCENT_RED : ACCENT_BLUE, fontFamily: 'var(--font-mono)', textDecoration: pathError ? 'line-through' : undefined }} aria-live="polite">
                {pathError ? destination : `${normalizedDest}/`}
              </Text>
            ) : (
              <Text style={ui(13, TEXT_MUTED)} aria-live="polite">
                bucket root
              </Text>
            )}
          </div>
          <div style={{ display: 'flex', gap: SPACE.xs }}>
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
            <Button icon={<FolderAddOutlined />} onClick={handleNewFolder}>
              New folder
            </Button>
          </div>
          {pathError && (
            <Text role="alert" style={{ ...ui(12, ACCENT_RED), display: 'block', marginTop: SPACE.xs }}>
              {pathError}
            </Text>
          )}
          {destReadOnly && (
            <div role="alert" style={{ ...ui(12, ACCENT_RED), marginTop: SPACE.xs }}>
              <div>
                You cannot upload to {bucket}/{normalizedDest ? `${normalizedDest}/` : ''}: your account has no write
                permission there.
              </div>
              {writablePrefixes.length > 0 && (
                <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: SPACE.xs, marginTop: SPACE.xs, color: TEXT_SECONDARY }}>
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
          <Text style={{ ...ui(12, TEXT_MUTED), display: 'block', marginTop: SPACE.xs }}>
            Edit the path above to change the destination folder. It is created if it doesn&rsquo;t exist.
          </Text>
        </div>

        {/* ONE toolbar for the files on this page (D33): add more, clear. */}
        {hasFiles && (
          <div role="toolbar" aria-label="Files" style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs, flexWrap: 'wrap', marginBottom: SPACE.sm }}>
            <Button size="small" icon={<PlusOutlined />} onClick={() => fileInputRef.current?.click()}>
              Add files
            </Button>
            <Button size="small" icon={<FolderAddOutlined />} onClick={() => folderInputRef.current?.click()}>
              Add folder
            </Button>
            {pendingFiles.length > 0 && (
              <Button size="small" type="text" icon={<DeleteOutlined />} onClick={clearStaged}>
                Clear list
              </Button>
            )}
            {settled > 0 && (
              <Button size="small" type="text" icon={<DeleteOutlined />} onClick={clearCompleted}>
                Clear finished uploads
              </Button>
            )}
          </div>
        )}

        {/* Staged files: nothing uploads until the user presses Upload. A
            neutral card: the accent tint is for a drag only (D34). */}
        {pendingFiles.length > 0 && (
          <section aria-label="Files to upload" style={card}>
            <Text style={{ ...ui(14, TEXT_PRIMARY), fontWeight: 600, display: 'block', marginBottom: SPACE.xxs }} role="status" aria-live="polite">
              {stagedHeadline(rows)}
            </Text>
            <Text style={{ ...ui(12, TEXT_SECONDARY), display: 'block', marginBottom: SPACE.sm }}>
              Nothing is uploaded yet. Set the destination folder above, check the list, then press Upload.
            </Text>
            <div style={{ position: 'relative', marginBottom: SPACE.sm }}>
              <ul
                ref={stagedListRef}
                aria-label="Staged files"
                onScroll={(e) => {
                  const el = e.currentTarget;
                  setStagedAtEnd(el.scrollHeight - el.scrollTop - el.clientHeight < 2);
                }}
                style={{
                  listStyle: 'none', margin: 0, padding: 0,
                  maxHeight: 'min(320px, 50vh)', overflow: 'auto',
                  borderRadius: SPACE.xs, border: `1px solid ${BORDER}`, background: BG_BASE,
                }}
              >
                {rows.slice(0, STAGED_ROWS_SHOWN).map((r) => {
                  const refusedRow = r.state === 'refused';
                  const replaced = replacedPaths.has(r.path);
                  return (
                    <li
                      key={stagedId(r.file)}
                      aria-label={refusedRow ? `${r.path}: refused` : r.path}
                      style={{ padding: `${SPACE.xxs}px ${SPACE.xxs}px ${SPACE.xxs}px ${SPACE.sm}px`, borderTop: `1px solid ${BORDER}`, marginTop: -1 }}
                    >
                      <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs }}>
                        {refusedRow && <CloseCircleFilled aria-hidden="true" style={{ color: ACCENT_RED, fontSize: 13, flexShrink: 0 }} />}
                        <TruncatedName
                          text={r.path}
                          style={{ flex: 1, fontFamily: 'var(--font-mono)', fontSize: 12, color: refusedRow ? ACCENT_RED : TEXT_PRIMARY }}
                        />
                        <span style={{ flexShrink: 0, fontFamily: 'var(--font-mono)', fontSize: 12, color: TEXT_MUTED, fontVariantNumeric: 'tabular-nums' }}>
                          {formatBytes(r.file.size)}
                        </span>
                        <Button
                          type="text"
                          size="small"
                          icon={<CloseOutlined />}
                          aria-label={`Remove ${r.path} from the list`}
                          title="Remove from the list"
                          onClick={() => unstage(r.file)}
                          style={{ flexShrink: 0 }}
                        />
                      </div>
                      {refusedRow && (
                        <div style={{ ...ui(12, ACCENT_RED), paddingBottom: SPACE.xxs }}>
                          Cannot be uploaded: {r.reason}.
                        </div>
                      )}
                      {replaced && !refusedRow && (
                        <div style={{ ...ui(12, ACCENT_AMBER), paddingBottom: SPACE.xxs }}>
                          Replaces the file with the same name that was in the list.
                        </div>
                      )}
                    </li>
                  );
                })}
                {rows.length > STAGED_ROWS_SHOWN && (
                  <li style={{ ...ui(12, TEXT_MUTED), padding: `${SPACE.xs}px ${SPACE.sm}px`, borderTop: `1px solid ${BORDER}` }}>
                    And {(rows.length - STAGED_ROWS_SHOWN).toLocaleString()} more files
                  </li>
                )}
              </ul>
              {!stagedAtEnd && (
                <div
                  aria-hidden="true"
                  style={{
                    position: 'absolute', left: 1, right: 1, bottom: 1, height: SPACE.xl,
                    borderRadius: `0 0 ${SPACE.xs}px ${SPACE.xs}px`,
                    background: `linear-gradient(to bottom, transparent, ${BG_BASE})`,
                    pointerEvents: 'none',
                  }}
                />
              )}
            </div>
            {shownRows > STAGED_ROWS_VISIBLE && (
              <Text style={{ ...ui(12, TEXT_MUTED), display: 'block', marginTop: -SPACE.xs, marginBottom: SPACE.sm }}>
                Scroll the list to see all {rows.length.toLocaleString()} files.
              </Text>
            )}
            {ready > 0 ? (
              <Space wrap>
                <Button
                  type="primary"
                  size="large"
                  icon={<CloudUploadOutlined />}
                  onClick={commitPending}
                  disabled={destError !== null}
                  style={{ fontWeight: 600 }}
                >
                  {uploadButtonLabel(ready, pathError ? null : destLabel)}
                </Button>
                {destError !== null && (
                  <Text style={ui(12, ACCENT_RED)}>Fix the destination folder to upload.</Text>
                )}
              </Space>
            ) : (
              <Text style={ui(12, TEXT_SECONDARY)}>
                Remove the refused files from the list, or free space in the bucket and add them again.
              </Text>
            )}
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
          <label htmlFor="new-folder-name" style={{ ...ui(13, TEXT_PRIMARY), display: 'block', marginBottom: SPACE.xs }}>
            Folder name
          </label>
          <Input
            id="new-folder-name"
            value={folderName}
            onChange={(e) => setFolderName(e.target.value)}
            onPressEnter={handleFolderConfirm}
            placeholder="my-folder"
            autoFocus
            style={{ fontFamily: 'var(--font-mono)' }}
            status={folderNameError ? 'error' : undefined}
          />
          {folderNameError && (
            <Text role="alert" style={{ ...ui(12, ACCENT_RED), display: 'block', marginTop: SPACE.xs }}>
              {folderNameError}
            </Text>
          )}
        </Modal>

        {/* Drop-zone box — only while there are no files in context yet. Once
            files are staged or uploading, the overlay below takes a drop. */}
        {!hasFiles && (
          // Not role="button": it holds the two real buttons (axe
          // nested-interactive). A click anywhere is a mouse shortcut to them.
          <div
            style={{
              border: `2px dashed ${dragging ? ACCENT_BLUE : BORDER}`,
              borderRadius: CARD_RADIUS,
              padding: `clamp(${SPACE.xl}px, 5vw, 48px) ${SPACE.lg}px`,
              textAlign: 'center',
              marginBottom: SPACE.lg,
              background: dragging ? 'var(--drop-glow)' : 'var(--glass-bg)',
              transition: 'border-color 0.25s ease, background-color 0.25s ease',
              cursor: 'pointer',
              animation: dragging ? 'dropGlow 2s ease-in-out infinite' : undefined,
            }}
            onClick={() => fileInputRef.current?.click()}
          >
            <CloudUploadOutlined aria-hidden="true" style={{ fontSize: 48, color: dragging ? ACCENT_BLUE : TEXT_MUTED, marginBottom: SPACE.md, transition: 'color 0.2s' }} />
            <div style={{ marginBottom: SPACE.sm }}>
              <Text style={{ ...ui(15, TEXT_PRIMARY), fontWeight: 500 }}>Drag and drop files here</Text>
            </div>
            <div style={{ marginBottom: SPACE.lg }}>
              <Text style={ui(12, TEXT_MUTED)}>or select files. You check the list before the upload starts.</Text>
            </div>
            <Space size={SPACE.sm} onClick={(e) => e.stopPropagation()}>
              <Button type="primary" icon={<CloudUploadOutlined />} onClick={() => fileInputRef.current?.click()}>
                Select files
              </Button>
              <Button icon={<FolderAddOutlined />} onClick={() => folderInputRef.current?.click()}>
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
            if (e.target.files?.length) stage(Array.from(e.target.files));
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
            if (e.target.files?.length) stage(Array.from(e.target.files));
            e.target.value = '';
          }}
        />

        {/* The outcome — ABOVE the progress list so "Done" is reachable
            without scrolling past every row. ONE state model: while files
            upload, the count; then the banner, green only when every file
            succeeded (D23). */}
        {queue.length > 0 && (
          <div
            ref={queueHeadingRef}
            tabIndex={-1}
            role="status"
            aria-live="polite"
            className={tone === 'success' ? 'dg-upload-success' : undefined}
            style={{
              ...card,
              outline: 'none',
              borderColor: showBanner ? toneColor : BORDER,
              display: 'flex',
              alignItems: 'center',
              gap: SPACE.md,
              flexWrap: 'wrap',
            }}
          >
            {showBanner ? (
              <>
                <ToneIcon aria-hidden="true" style={{ fontSize: 28, color: toneColor }} />
                <div style={{ flex: '1 1 240px', minWidth: 0 }}>
                  <div style={{ ...ui(16, TEXT_PRIMARY), fontWeight: 700 }}>{headline.title}</div>
                  <div style={{ ...ui(13, TEXT_SECONDARY), overflowWrap: 'anywhere' }}>{headline.body}</div>
                </div>
                <Space wrap>
                  {outcome.retryable > 0 && (
                    <Button icon={<ReloadOutlined />} onClick={retryFailed}>
                      Retry failed
                    </Button>
                  )}
                  {outcome.done > 0 ? (
                    <Button type="primary" size="large" icon={<CheckCircleFilled />} onClick={finish} style={{ fontWeight: 600 }}>
                      Done
                    </Button>
                  ) : (
                    <Button icon={<ArrowLeftOutlined />} onClick={handleBack}>
                      Back to browser
                    </Button>
                  )}
                </Space>
              </>
            ) : (
              <>
                <CloudUploadOutlined aria-hidden="true" style={{ fontSize: 24, color: ACCENT_BLUE }} />
                <div style={{ flex: '1 1 240px', ...ui(14, TEXT_PRIMARY), fontWeight: 600 }}>
                  Uploading: {settled.toLocaleString()} of {outcome.total.toLocaleString()} files finished
                  {outcome.failed > 0 ? `, ${outcome.failed.toLocaleString()} failed` : ''}
                </div>
                <Button icon={<ArrowLeftOutlined />} onClick={handleBack}>
                  Back to browser
                </Button>
              </>
            )}
          </div>
        )}
        {queue.length === 0 && (
          <div style={{ marginBottom: SPACE.lg }}>
            <Button icon={<ArrowLeftOutlined />} onClick={handleBack}>
              Back to browser
            </Button>
          </div>
        )}

        {/* Session figures — once a file landed; a wall of zeros is noise. */}
        {stats.uploaded > 0 && (
          <div style={{ marginBottom: SPACE.lg }}>
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))', gap: SPACE.sm }}>
              {sessionTiles.map((stat) => (
                <div key={stat.label} title={stat.hint} className="glass-card" style={{ borderRadius: CARD_RADIUS, padding: `${SPACE.sm}px ${SPACE.md}px` }}>
                  <Text style={{ ...ui(12, TEXT_MUTED), display: 'block', marginBottom: SPACE.xxs }}>{stat.label}</Text>
                  <Text style={{ fontSize: 20, fontWeight: 700, color: stat.color ?? TEXT_PRIMARY, fontFamily: 'var(--font-mono)' }}>{stat.value}</Text>
                </div>
              ))}
            </div>
            {stats.baselineCount > 0 && stats.storedSize !== null && (
              <Text style={{ ...ui(12, TEXT_MUTED), display: 'block', marginTop: SPACE.sm }}>
                {stats.baselineCount === 1 ? 'One upload was' : `${stats.baselineCount} uploads were`} the first
                file in {stats.baselineCount === 1 ? 'its folder' : 'their folders'}. The first file is stored in
                full as the folder&apos;s baseline, and later versions are stored as small deltas against it. The
                figures above include {stats.baselineCount === 1 ? 'that baseline' : 'those baselines'}, so the
                savings grow as you upload more versions.
              </Text>
            )}
          </div>
        )}

        {/* Upload queue: scrolls past ~6 rows and renders only the rows in view. */}
        {queue.length > 0 && (
          <div style={{ marginBottom: SPACE.lg }}>
            <Text style={{ ...ui(14, TEXT_PRIMARY), fontWeight: 600, display: 'block', marginBottom: SPACE.sm }}>
              Uploads ({queue.length.toLocaleString()})
            </Text>
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
              onRemoveUpload={removeUpload}
            />
          </div>
        )}
      </div>

      {/* Drop feedback once the drop zone is gone (D26): the whole page takes
          the drop, and says so while files are dragged over it. */}
      {dragging && hasFiles && (
        <div
          aria-hidden="true"
          style={{
            position: 'absolute', inset: SPACE.xs, zIndex: 5, pointerEvents: 'none',
            border: `2px dashed ${ACCENT_BLUE}`, borderRadius: CARD_RADIUS,
            background: 'var(--drop-glow)',
            display: 'flex', alignItems: 'center', justifyContent: 'center',
          }}
        >
          <div style={{ ...ui(16, TEXT_PRIMARY), fontWeight: 600, background: BG_ELEVATED, border: `1px solid ${BORDER}`, borderRadius: CARD_RADIUS, padding: `${SPACE.sm}px ${SPACE.lg}px`, display: 'flex', alignItems: 'center', gap: SPACE.xs }}>
            <CloudUploadOutlined style={{ color: ACCENT_BLUE }} />
            Drop to add to the list
          </div>
        </div>
      )}
    </div>
  );
}
