import { useState, useEffect, useRef } from 'react';
import { Drawer, Button, Modal, message, Tag, Skeleton, Input, Spin } from 'antd';
import { DownloadOutlined, DeleteOutlined, LinkOutlined, FileOutlined, CloseOutlined, CheckCircleFilled, CopyOutlined, LoadingOutlined, EyeOutlined } from '@ant-design/icons';
import { deleteObject, downloadObject, getPresignedUrl, getObjectUrl, headObject, getBucket } from '../s3client';
import { GlobalOutlined } from '@ant-design/icons';
import { formatBytes, getFileName, downloadBlobAsFile } from '../utils';
import { summarizeObjectSavings } from '../savings';
import type { S3Object } from '../types';
import { useColors } from '../ThemeContext';
import { getPreviewMode } from './filePreviewMode';
import { useAdminConfig } from '../queries/config';
import { useOnClickOutside } from '../useDocumentEvent';

const SHARE_DURATIONS = [
  { label: '1 hour', seconds: 3600 },
  { label: '24 hours', seconds: 86400 },
  { label: '7 days', seconds: 7 * 24 * 3600 - 1 },
];

/** Centered vertical stack used by every loading/result modal body. */
const MODAL_CENTER_STACK = { display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 16 } as const;

/** Themed spinner used in the storage-stats and download/share modal states. */
function LoadingSpinner({ size, color }: { size: number; color: string }) {
  return <Spin indicator={<LoadingOutlined style={{ fontSize: size, color }} />} />;
}

interface BucketPolicyInfo {
  compressionEnabled: boolean;
  publicPrefixes: string[];
}

interface Props {
  object: S3Object | null;
  onClose: () => void;
  onDeleted: () => void;
  onPreview?: (obj: S3Object) => void;
  isMobile?: boolean;
  headCache?: Record<string, { storageType?: string; storedSize?: number; error?: boolean }>;
  canDelete?: boolean;
  canRead?: boolean;
  /** When false, skip loading full bucket policy from Settings; public-prefix hint stays hidden. */
  hasAdminSession?: boolean;
}

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

/** Section heading for inspector panels. Defined at module level to avoid React reconciliation issues. */
function InspectorSection({ title, children }: { title: string; children: React.ReactNode }) {
  const { TEXT_MUTED } = useColors();
  return (
    <section style={{ marginBottom: 20 }} aria-label={title}>
      <h3 style={{
        fontSize: 10,
        fontWeight: 700,
        letterSpacing: 1.5,
        textTransform: 'uppercase',
        color: TEXT_MUTED,
        marginBottom: 10,
        margin: '0 0 10px',
        fontFamily: "var(--font-ui)",
      }}>
        {title}
      </h3>
      {children}
    </section>
  );
}

/** Key-value row for inspector metadata. Defined at module level to avoid React reconciliation issues. */
function InfoRow({ label, value }: { label: string; value: string }) {
  const { BG_SIDEBAR, TEXT_MUTED, TEXT_PRIMARY } = useColors();
  return (
    <div style={{ padding: '8px 12px', background: BG_SIDEBAR, borderRadius: 8, marginBottom: 4 }}>
      <div style={{ fontSize: 11, color: TEXT_MUTED, marginBottom: 2, fontFamily: "var(--font-ui)", fontWeight: 500 }}>{label}</div>
      <div style={{ fontSize: 13, color: TEXT_PRIMARY, wordBreak: 'break-all', fontFamily: "var(--font-mono)" }}>{value}</div>
    </div>
  );
}

/** Split button: main button generates a 7-day link, chevron opens duration picker. */
function ShareDurationButton({
  durations,
  onSelect,
  accentBlue,
  border,
  textMuted,
  textPrimary,
  bgSidebar,
}: {
  durations: { label: string; seconds: number }[];
  onSelect: (seconds: number) => void;
  accentBlue: string;
  border: string;
  textMuted: string;
  textPrimary: string;
  bgSidebar: string;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useOnClickOutside([ref], () => setOpen(false), open);

  return (
    <div ref={ref} style={{ display: 'flex', width: '100%' }}>
      {/* Main button — default 7 days */}
      <button
        onClick={() => { onSelect(durations[durations.length - 1].seconds); setOpen(false); }}
        style={{
          flex: 1,
          height: 40,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          gap: 6,
          background: accentBlue,
          color: '#fff',
          border: 'none',
          borderRadius: '10px 0 0 10px',
          fontWeight: 600,
          fontSize: 14,
          fontFamily: 'var(--font-ui)',
          cursor: 'pointer',
        }}
        aria-label="Share link (7 days)"
      >
        <LinkOutlined /> Share
      </button>
      {/* Chevron toggle */}
      <button
        onClick={() => setOpen(!open)}
        aria-label="Choose link duration"
        aria-expanded={open}
        style={{
          width: 32,
          height: 40,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          background: accentBlue,
          color: '#fff',
          border: 'none',
          borderLeft: '1px solid rgba(255,255,255,0.25)',
          borderRadius: '0 10px 10px 0',
          cursor: 'pointer',
          fontSize: 10,
        }}
      >
        <span style={{ transform: open ? 'rotate(180deg)' : 'none', transition: 'transform 0.15s' }}>&#9660;</span>
      </button>
      {/* Duration dropdown */}
      {open && (
        <div
          role="listbox"
          aria-label="Link expiry duration"
          style={{
            position: 'absolute',
            top: '100%',
            left: 0,
            right: 0,
            marginTop: 4,
            background: bgSidebar,
            border: `1px solid ${border}`,
            borderRadius: 8,
            boxShadow: '0 8px 24px rgba(0,0,0,0.35)',
            zIndex: 10,
            overflow: 'hidden',
          }}
        >
          {durations.map((d) => (
            <div
              key={d.seconds}
              role="option"
              tabIndex={0}
              aria-selected={false}
              onClick={() => { onSelect(d.seconds); setOpen(false); }}
              onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onSelect(d.seconds); setOpen(false); } }}
              style={{
                padding: '10px 14px',
                cursor: 'pointer',
                fontSize: 13,
                fontFamily: 'var(--font-ui)',
                color: textPrimary,
                display: 'flex',
                justifyContent: 'space-between',
                alignItems: 'center',
                borderBottom: `1px solid ${border}`,
              }}
              onMouseEnter={(e) => { (e.currentTarget as HTMLDivElement).style.background = `${accentBlue}18`; }}
              onMouseLeave={(e) => { (e.currentTarget as HTMLDivElement).style.background = 'transparent'; }}
            >
              <span>{d.label}</span>
              <span style={{ fontSize: 11, color: textMuted, fontFamily: 'var(--font-mono)' }}>
                <LinkOutlined style={{ marginRight: 4 }} />Share
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export default function InspectorPanel({
  object,
  onClose,
  onDeleted,
  onPreview,
  isMobile,
  headCache,
  canDelete = true,
  canRead = true,
  hasAdminSession = true,
}: Props) {
  const {
    BG_SIDEBAR, BORDER, TEXT_PRIMARY, TEXT_MUTED, TEXT_FAINT,
    ACCENT_BLUE, ACCENT_GREEN, ACCENT_RED, STORAGE_TYPE_COLORS, STORAGE_TYPE_DEFAULT,
  } = useColors();
  const [messageApi, contextHolder] = message.useMessage();

  const [headData, setHeadData] = useState<{ headers: Record<string, string>; storageType?: string; storedSize?: number } | null>(null);
  const [headLoading, setHeadLoading] = useState(false);
  const [headReadable, setHeadReadable] = useState(false);

  // Modal state for download / share operations (must be declared before early return)
  const [modalState, setModalState] = useState<
    | { mode: 'download'; phase: 'loading' | 'ready' | 'error'; error?: string }
    | { mode: 'share'; phase: 'loading' | 'ready' | 'error'; url?: string; error?: string }
    | null
  >(null);
  const blobRef = useRef<{ blob: Blob; name: string } | null>(null);
  const [shareDuration, setShareDuration] = useState<number | null>(null);
  const objectKey = object?.key;
  const cachedHead = objectKey ? headCache?.[objectKey] : undefined;

  // Request-identity guard for the async button handlers (download / share /
  // delete). Same root cause and fix as the headObject + bucketPolicy effect
  // cancellation guards below: a handler captures `object.key` at call time,
  // but the `object` prop can change (user clicks a different row) while the
  // request is in flight. Without a guard, the stale promise resolves and
  // commits state (blobRef / modalState / onClose) against the wrong object.
  // We can't use a per-call `cancelled` flag like an effect because these are
  // event handlers, so instead we keep a ref pointing at the *currently*
  // selected key and bail any commit whose captured key no longer matches.
  const latestKeyRef = useRef<string | undefined>(objectKey);
  useEffect(() => { latestKeyRef.current = objectKey; }, [objectKey]);

  // Bucket policy (compression + public prefixes) is derived from the shared,
  // cached admin config — no bespoke per-bucket fetch effect. Skip the query
  // when the user has not signed in through Settings (a fetch would 403).
  //
  // `getBucket()` is a non-reactive module getter, so we read it during render
  // into `currentBucket`. The previous effect tracked bucket changes manually
  // and cancelled stale fetches; with a cached query the active bucket's policy
  // is just a lookup against `config.bucket_policies`, recomputed each render.
  const { data: adminConfig, isLoading: adminConfigLoading } = useAdminConfig({
    enabled: hasAdminSession,
  });
  const currentBucket = getBucket();
  const bucketPolicy: BucketPolicyInfo | null = (() => {
    if (!hasAdminSession || !currentBucket || !adminConfig) return null;
    const bp =
      adminConfig.bucket_policies?.[currentBucket] ||
      adminConfig.bucket_policies?.[currentBucket.toLowerCase()];
    // Match `BucketPolicyRegistry::compression_enabled`: per-bucket `compression`
    // only, then default `true`. Do not infer from `max_delta_ratio` — global
    // ratio 0 is a *threshold* / passthrough decision, not the same as
    // `compression: false` on the bucket.
    return {
      compressionEnabled: bp?.compression ?? true,
      publicPrefixes: bp?.public_prefixes ?? [],
    };
  })();
  // True until the policy for the active bucket is resolvable: still loading the
  // config, while we have an admin session and a bucket but no config yet.
  const bucketPolicyLoading =
    hasAdminSession && !!currentBucket && !adminConfig && adminConfigLoading;

  useEffect(() => {
    if (!objectKey) { setHeadData(null); setHeadReadable(false); return; }
    // Cancellation guard: rapid object switching previously let an older
    // headObject() resolve into the newer object's drawer, showing stale
    // metadata for the wrong file. Capture a flag, only commit when alive.
    let cancelled = false;
    setHeadReadable(false);
    // Seed from table's headCache so Storage Stats renders instantly
    if (cachedHead) {
      setHeadData({ headers: {}, storageType: cachedHead.storageType, storedSize: cachedHead.storedSize });
    } else {
      setHeadData(null);
    }
    setHeadLoading(true);
    headObject(objectKey)
      .then((data) => {
        if (!cancelled) {
          setHeadData(data);
          setHeadReadable(true);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setHeadReadable(false);
          setHeadData((prev) => prev ?? { headers: {} });
        }
      })
      .finally(() => { if (!cancelled) setHeadLoading(false); });
    return () => { cancelled = true; };
  }, [cachedHead, objectKey]);

  if (!object) return null;

  const fileName = getFileName(object.key);
  const headers = headData?.headers ?? {};
  const storageType = headData?.storageType;
  const storedSize = headData?.storedSize;
  // Original size: from HEAD metadata (dg-file-size) when available, else from LIST.
  // LIST returns the *stored* (compressed) size for delta objects ("Lite LIST optimization"),
  // so object.size is NOT the original size for deltas. The HEAD metadata has the real original.
  const dgFileSize = headers['x-amz-meta-dg-file-size'];
  const originalSize = dgFileSize ? parseInt(dgFileSize, 10) : object.size;
  // All "savings" math goes through `src/savings.ts` — the single
  // client-side formula. Pre-centralisation the inline math here
  // differed from useUploadQueue + DeltaSavingsChip in both cap (99.9 vs 99)
  // and zero-handling, so two surfaces could report different "saved %"
  // for the same data.
  const objectSavings = summarizeObjectSavings(originalSize, storedSize);
  const savings = objectSavings.pct;
  const savedBytes = objectSavings.savedBytes;

  const dgMeta = getDgMetadata(headers);
  const userMeta = getUserMetadata(headers);

  const storageTypeLabel = storageType || 'Original';
  const storageTypeColor = STORAGE_TYPE_COLORS[storageType || 'passthrough'] || STORAGE_TYPE_DEFAULT;
  // Once the HEAD resolves, an object stored as `passthrough` (e.g. a `.sha512`
  // checksum sidecar, an already-compressed asset, or any non-delta-eligible
  // file) has NO compression to report — stored == original. Showing a
  // "Savings %" / original-vs-stored panel for it is meaningless, and the panel
  // would otherwise sit on its loading spinner waiting for delta stats that
  // never come. Treat passthrough like "compression off": render the simple
  // size view instead. Until the HEAD resolves (storageType still undefined),
  // keep the bucket-policy default so we don't flicker.
  const isPassthroughObject = storageType === 'passthrough';
  const compressionEnabled = (bucketPolicy?.compressionEnabled ?? true) && !isPassthroughObject;
  /** When policy is still loading or unavailable, never show a false "public" badge. */
  const isPublic =
    hasAdminSession &&
    !bucketPolicyLoading &&
    (bucketPolicy?.publicPrefixes.some(pp => pp === '' || object.key.startsWith(pp)) ?? false);
  // Non-admin IAM users connect with S3 credentials but may not have a GUI
  // session, so /whoami cannot always provide permissions. A successful HEAD
  // proves object-level read without exposing controls to list-only users.
  const canReadObject = canRead || headReadable;

  const handleDelete = async () => {
    const reqKey = object.key;
    try {
      await deleteObject(reqKey);
      // If the user switched to a different object mid-delete, still refresh the
      // list (A really was deleted) but do NOT deselect the now-current object B.
      if (latestKeyRef.current !== reqKey) { onDeleted(); return; }
      onClose();
      onDeleted();
    } catch {
      messageApi.error('Failed to delete object');
    }
  };

  const handleDownload = async () => {
    const reqKey = object.key;
    const reqName = fileName;
    setModalState({ mode: 'download', phase: 'loading' });
    blobRef.current = null;
    try {
      const blob = await downloadObject(reqKey);
      // Stale-commit guard: drop the result if the selection changed mid-flight.
      if (latestKeyRef.current !== reqKey) return;
      blobRef.current = { blob, name: reqName };
      setModalState({ mode: 'download', phase: 'ready' });
    } catch (e) {
      if (latestKeyRef.current !== reqKey) return;
      setModalState({ mode: 'download', phase: 'error', error: String(e) });
    }
  };

  const triggerBlobDownload = () => {
    if (!blobRef.current) return;
    downloadBlobAsFile(blobRef.current.blob, blobRef.current.name);
    setModalState(null);
  };

  const handleCopyLink = async (expiresInSeconds?: number) => {
    const reqKey = object.key;
    setModalState({ mode: 'share', phase: 'loading' });
    setShareDuration(expiresInSeconds ?? SHARE_DURATIONS[2].seconds);
    try {
      let url: string;
      try {
        url = await getPresignedUrl(reqKey, expiresInSeconds);
      } catch (e) {
        console.warn('Presigned URL failed, falling back to direct URL:', e);
        url = getObjectUrl(reqKey);
      }
      // Stale-commit guard: don't surface A's URL after the user moved to B.
      if (latestKeyRef.current !== reqKey) return;
      setModalState({ mode: 'share', phase: 'ready', url });
    } catch (e) {
      if (latestKeyRef.current !== reqKey) return;
      setModalState({ mode: 'share', phase: 'error', error: String(e) });
    }
  };

  const handleCopyUrl = async () => {
    if (modalState?.mode === 'share' && modalState.url) {
      await navigator.clipboard.writeText(modalState.url);
      messageApi.success('Link copied');
    }
  };

  return (
    <>
      {contextHolder}
      <Drawer
        placement="right"
        size={isMobile ? '100%' : 380}
        open={!!object}
        onClose={onClose}
        closable={false}
        title={<span className="sr-only">Object inspector: {fileName}</span>}
        styles={{
          body: { padding: 0, display: 'flex', flexDirection: 'column' },
          header: { display: 'none' },
        }}
      >
        <div className="animate-slide-in" style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
          {/* Header */}
          <div style={{
            padding: '16px 20px',
            borderBottom: `1px solid ${BORDER}`,
            display: 'flex',
            alignItems: 'flex-start',
            gap: 12,
          }}>
            <div style={{
              width: 40,
              height: 40,
              borderRadius: 10,
              background: `linear-gradient(135deg, ${ACCENT_BLUE}15, ${ACCENT_BLUE}08)`,
              border: `1px solid ${ACCENT_BLUE}22`,
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              flexShrink: 0,
            }}>
              <FileOutlined aria-hidden="true" style={{ fontSize: 20, color: ACCENT_BLUE }} />
            </div>
            <div style={{ flex: 1, minWidth: 0 }}>
              <h2 style={{ fontSize: 15, fontWeight: 600, color: TEXT_PRIMARY, wordBreak: 'break-all', margin: 0, fontFamily: "var(--font-ui)" }}>
                {fileName}
              </h2>
              <div style={{ fontSize: 11, color: TEXT_MUTED, marginTop: 2, wordBreak: 'break-all', fontFamily: "var(--font-mono)" }}>
                {object.key}
              </div>
            </div>
            <Button
              type="text"
              icon={<CloseOutlined />}
              onClick={onClose}
              size="small"
              aria-label="Close inspector"
              style={{ color: TEXT_MUTED, flexShrink: 0 }}
            />
          </div>

          {/* Content */}
          <div style={{ flex: 1, overflow: 'auto', padding: '16px 20px' }}>
            {/* Preview button (only for previewable files) */}
            {canReadObject && onPreview && object && getPreviewMode(object.key) && (
              <Button
                block
                size="large"
                icon={<EyeOutlined />}
                onClick={() => onPreview(object)}
                style={{
                  fontWeight: 600,
                  borderRadius: 10,
                  fontFamily: "var(--font-ui)",
                  marginBottom: 8,
                }}
              >
                Preview
              </Button>
            )}

            {/* Download & Share buttons */}
            {canReadObject && (
              <div style={{ display: 'flex', gap: 8, marginBottom: 20 }}>
                <Button
                  type="primary"
                  size="large"
                  icon={<DownloadOutlined />}
                  onClick={handleDownload}
                  style={{
                    flex: 1,
                    background: ACCENT_GREEN,
                    borderColor: ACCENT_GREEN,
                    fontWeight: 600,
                    borderRadius: 10,
                    fontFamily: "var(--font-ui)",
                  }}
                >
                  Download
                </Button>
                <div style={{ flex: 1, position: 'relative' }}>
                  <ShareDurationButton
                    durations={SHARE_DURATIONS}
                    onSelect={(seconds) => handleCopyLink(seconds)}
                    accentBlue={ACCENT_BLUE}
                    border={BORDER}
                    textMuted={TEXT_MUTED}
                    textPrimary={TEXT_PRIMARY}
                    bgSidebar={BG_SIDEBAR}
                  />
                </div>
              </div>
            )}

            {/* PUBLIC ACCESS BADGE */}
            {isPublic && (
              <div style={{
                display: 'flex', alignItems: 'center', gap: 6,
                padding: '8px 12px', marginBottom: 12,
                background: `${ACCENT_BLUE}10`, border: `1px solid ${ACCENT_BLUE}30`,
                borderRadius: 8, fontSize: 12, color: ACCENT_BLUE,
                fontFamily: 'var(--font-ui)', fontWeight: 600,
              }}>
                <GlobalOutlined style={{ fontSize: 14 }} />
                Publicly accessible — no authentication required
              </div>
            )}

            {/* STORAGE STATS */}
            {bucketPolicyLoading ? (
              <InspectorSection title="Storage Stats">
                <div style={{
                  background: BG_SIDEBAR, borderRadius: 10, padding: '24px 16px',
                  display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 8,
                }}>
                  <LoadingSpinner size={28} color={ACCENT_GREEN} />
                  <div style={{ fontSize: 11, color: TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
                    Loading bucket policy…
                  </div>
                </div>
              </InspectorSection>
            ) : compressionEnabled ? (
              <InspectorSection title="Storage Stats">
                {headLoading && Object.keys(headers).length === 0 && storedSize == null ? (
                  /* Spin only while the HEAD is in flight AND we have nothing
                   * renderable yet. If the table's headCache already seeded a
                   * storedSize/storageType, render immediately instead of
                   * spinning behind an in-flight HEAD that may be slow. */
                  <div style={{
                    background: BG_SIDEBAR, borderRadius: 10, padding: '24px 16px',
                    display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 8,
                  }}>
                    <LoadingSpinner size={28} color={ACCENT_GREEN} />
                    <div style={{ fontSize: 11, color: TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
                      Loading compression stats...
                    </div>
                  </div>
                ) : (
                  <div style={{
                    background: BG_SIDEBAR,
                    borderRadius: 10,
                    padding: '16px',
                    textAlign: 'center',
                  }}>
                    <div style={{ fontSize: 10, fontWeight: 700, letterSpacing: 1, color: TEXT_MUTED, textTransform: 'uppercase', marginBottom: 4, fontFamily: "var(--font-ui)" }}>
                      Savings
                    </div>
                    <div style={{ fontSize: 32, fontWeight: 800, color: ACCENT_GREEN, lineHeight: 1.1, fontFamily: "var(--font-mono)" }}>
                      {savings.toFixed(1)}%
                    </div>
                    <div style={{ fontSize: 11, color: TEXT_MUTED, marginBottom: 12, fontFamily: "var(--font-mono)" }}>
                      {formatBytes(savedBytes)} saved
                    </div>
                    {/* Visual comparison bar */}
                    <div style={{ marginBottom: 10 }}>
                      <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 4 }}>
                        <div style={{ fontSize: 11, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Original</div>
                        <div style={{ fontSize: 12, fontWeight: 600, color: TEXT_PRIMARY, fontFamily: "var(--font-mono)" }}>{formatBytes(originalSize)}</div>
                      </div>
                      <div style={{ height: 6, borderRadius: 3, background: `${TEXT_FAINT}33`, overflow: 'hidden' }}>
                        <div style={{ height: '100%', borderRadius: 3, width: '100%', background: `${TEXT_MUTED}66` }} />
                      </div>
                    </div>
                    <div style={{ marginBottom: 8 }}>
                      <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 4 }}>
                        <div style={{ fontSize: 11, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Stored</div>
                        <div style={{ fontSize: 12, fontWeight: 600, color: TEXT_PRIMARY, fontFamily: "var(--font-mono)" }}>
                          {storedSize != null ? formatBytes(storedSize) : formatBytes(originalSize)}
                        </div>
                      </div>
                      <div style={{ height: 6, borderRadius: 3, background: `${TEXT_FAINT}33`, overflow: 'hidden' }}>
                        <div style={{
                          height: '100%',
                          borderRadius: 3,
                          width: `${storedSize != null && originalSize > 0 ? Math.max(2, (storedSize / originalSize) * 100) : 100}%`,
                          background: ACCENT_GREEN,
                          transition: 'width 0.4s ease-out',
                        }} />
                      </div>
                    </div>
                    <Tag style={{
                      background: storageTypeColor.bg,
                      border: `1px solid ${storageTypeColor.border}`,
                      color: storageTypeColor.text,
                      fontSize: 12,
                      borderRadius: 6,
                      fontFamily: "var(--font-mono)",
                    }}>
                      {storageTypeLabel.charAt(0).toUpperCase() + storageTypeLabel.slice(1)}
                    </Tag>
                  </div>
                )}
              </InspectorSection>
            ) : (
              /* Compression disabled for this bucket — show clean, simple size info */
              <InspectorSection title="Storage">
                <div style={{
                  background: BG_SIDEBAR, borderRadius: 10, padding: '16px', textAlign: 'center',
                }}>
                  <div style={{ fontSize: 11, color: TEXT_MUTED, fontFamily: 'var(--font-ui)', marginBottom: 4 }}>Size</div>
                  <div style={{ fontSize: 28, fontWeight: 700, color: TEXT_PRIMARY, fontFamily: 'var(--font-mono)', lineHeight: 1.2 }}>
                    {formatBytes(object.size)}
                  </div>
                  <div style={{ fontSize: 11, color: TEXT_FAINT, fontFamily: 'var(--font-ui)', marginTop: 8 }}>
                    Compression disabled for this bucket
                  </div>
                </div>
              </InspectorSection>
            )}

            {/* OBJECT INFO */}
            <InspectorSection title="Object Info">
              <InfoRow
                label="Last modified"
                value={object.lastModified ? new Date(object.lastModified).toLocaleString() : '--'}
              />
              <InfoRow label="Accept-Ranges" value="Disabled" />
            </InspectorSection>

            {/* S3 METADATA */}
            <InspectorSection title="S3 Metadata">
              {headLoading ? (
                <Skeleton active paragraph={{ rows: 1 }} />
              ) : (
                <InfoRow
                  label="Content-Type"
                  value={headers['content-type'] || 'binary/octet-stream'}
                />
              )}
            </InspectorSection>

            {/* CUSTOM METADATA (DG + User) */}
            <InspectorSection title="Custom Metadata">
              {headLoading ? (
                <Skeleton active paragraph={{ rows: 2 }} />
              ) : dgMeta.length === 0 && userMeta.length === 0 ? (
                <div style={{ fontSize: 12, color: TEXT_FAINT, display: 'flex', alignItems: 'center', gap: 6, fontFamily: "var(--font-ui)" }}>
                  No custom metadata
                </div>
              ) : (
                <>
                  {dgMeta.map(([k, v]) => <InfoRow key={k} label={`dg-${k}`} value={v} />)}
                  {userMeta.map(([k, v]) => <InfoRow key={k} label={k} value={v} />)}
                </>
              )}
            </InspectorSection>

            {/* TAGS */}
            <InspectorSection title="Tags">
              <div style={{ fontSize: 12, color: TEXT_FAINT, display: 'flex', alignItems: 'center', gap: 6, fontFamily: "var(--font-ui)" }}>
                No tags available
              </div>
            </InspectorSection>
          </div>

          {canDelete && (
            <div style={{ padding: '16px 20px', borderTop: `1px solid ${BORDER}` }}>
              <Button
                block
                icon={<DeleteOutlined />}
                onClick={handleDelete}
                style={{
                  background: 'transparent',
                  borderColor: BORDER,
                  color: ACCENT_RED,
                  borderRadius: 10,
                  fontFamily: "var(--font-ui)",
                  fontWeight: 600,
                }}
              >
                Delete object
              </Button>
            </div>
          )}
        </div>
      </Drawer>

      {/* Download / Share modal */}
      <Modal
        open={!!modalState}
        onCancel={() => { setModalState(null); blobRef.current = null; }}
        footer={null}
        centered
        width={420}
        closable={modalState?.phase !== 'loading'}
        mask={{ closable: modalState?.phase !== 'loading' }}
        styles={{ body: { padding: '32px 24px', textAlign: 'center' } }}
      >
        {modalState?.mode === 'download' && (
          <>
            {modalState.phase === 'loading' && (
              <div style={MODAL_CENTER_STACK}>
                <LoadingSpinner size={40} color={ACCENT_GREEN} />
                <div>
                  <div style={{ fontSize: 16, fontWeight: 600, color: TEXT_PRIMARY, marginBottom: 6, fontFamily: "var(--font-ui)" }}>
                    Reconstructing file…
                  </div>
                  <div style={{ fontSize: 13, color: TEXT_MUTED, lineHeight: 1.5, fontFamily: "var(--font-ui)" }}>
                    The proxy is assembling the original file from its
                    delta-compressed storage. This may take a moment for
                    large files.
                  </div>
                  <div style={{ fontSize: 12, color: TEXT_FAINT, marginTop: 8, fontFamily: "var(--font-mono)" }}>
                    {fileName} · {formatBytes(object.size)}
                  </div>
                </div>
              </div>
            )}
            {modalState.phase === 'ready' && (
              <div style={MODAL_CENTER_STACK}>
                <CheckCircleFilled style={{ fontSize: 40, color: ACCENT_GREEN }} />
                <div>
                  <div style={{ fontSize: 16, fontWeight: 600, color: TEXT_PRIMARY, marginBottom: 6, fontFamily: "var(--font-ui)" }}>
                    File ready
                  </div>
                  <div style={{ fontSize: 12, color: TEXT_FAINT, fontFamily: "var(--font-mono)" }}>
                    {fileName} · {formatBytes(object.size)}
                  </div>
                </div>
                <Button
                  type="primary"
                  size="large"
                  icon={<DownloadOutlined />}
                  onClick={triggerBlobDownload}
                  style={{
                    background: ACCENT_GREEN,
                    borderColor: ACCENT_GREEN,
                    fontWeight: 600,
                    borderRadius: 10,
                    fontFamily: "var(--font-ui)",
                    minWidth: 180,
                  }}
                >
                  Save file
                </Button>
              </div>
            )}
            {modalState.phase === 'error' && (
              <div style={MODAL_CENTER_STACK}>
                <DeleteOutlined style={{ fontSize: 40, color: ACCENT_RED }} />
                <div>
                  <div style={{ fontSize: 16, fontWeight: 600, color: ACCENT_RED, marginBottom: 6, fontFamily: "var(--font-ui)" }}>
                    Download failed
                  </div>
                  <div style={{ fontSize: 13, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
                    {modalState.error || 'An unexpected error occurred'}
                  </div>
                </div>
              </div>
            )}
          </>
        )}

        {modalState?.mode === 'share' && (
          <>
            {modalState.phase === 'loading' && (
              <div style={MODAL_CENTER_STACK}>
                <LoadingSpinner size={40} color={ACCENT_BLUE} />
                <div>
                  <div style={{ fontSize: 16, fontWeight: 600, color: TEXT_PRIMARY, marginBottom: 6, fontFamily: "var(--font-ui)" }}>
                    Generating signed link…
                  </div>
                  <div style={{ fontSize: 13, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
                    Creating a pre-signed URL for direct access.
                  </div>
                </div>
              </div>
            )}
            {modalState.phase === 'ready' && (
              <div style={MODAL_CENTER_STACK}>
                <CheckCircleFilled style={{ fontSize: 40, color: ACCENT_BLUE }} />
                <div style={{ width: '100%' }}>
                  <div style={{ fontSize: 16, fontWeight: 600, color: TEXT_PRIMARY, marginBottom: 4, fontFamily: "var(--font-ui)" }}>
                    Signed link ready
                  </div>
                  <div style={{ fontSize: 12, color: TEXT_MUTED, marginBottom: 12, fontFamily: "var(--font-ui)" }}>
                    Expires in {SHARE_DURATIONS.find(d => d.seconds === shareDuration)?.label ?? 'unknown'}
                  </div>
                  <Input.TextArea
                    value={modalState.url}
                    readOnly
                    autoSize={{ minRows: 2, maxRows: 4 }}
                    style={{
                      fontFamily: "var(--font-mono)",
                      fontSize: 12,
                      borderRadius: 8,
                      marginBottom: 12,
                    }}
                  />
                  <Button
                    type="primary"
                    icon={<CopyOutlined />}
                    onClick={handleCopyUrl}
                    style={{
                      fontWeight: 600,
                      borderRadius: 10,
                      fontFamily: "var(--font-ui)",
                      minWidth: 180,
                    }}
                  >
                    Copy link
                  </Button>
                </div>
              </div>
            )}
            {modalState.phase === 'error' && (
              <div style={MODAL_CENTER_STACK}>
                <DeleteOutlined style={{ fontSize: 40, color: ACCENT_RED }} />
                <div>
                  <div style={{ fontSize: 16, fontWeight: 600, color: ACCENT_RED, marginBottom: 6, fontFamily: "var(--font-ui)" }}>
                    Failed to generate link
                  </div>
                  <div style={{ fontSize: 13, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
                    {modalState.error || 'An unexpected error occurred'}
                  </div>
                </div>
              </div>
            )}
          </>
        )}
      </Modal>
    </>
  );
}
