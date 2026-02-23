import { useState, useEffect, useRef } from 'react';
import { Drawer, Button, Modal, message, Tag, Skeleton, Input, Spin } from 'antd';
import { DownloadOutlined, DeleteOutlined, LinkOutlined, FileOutlined, CloseOutlined, CheckCircleFilled, CopyOutlined, LoadingOutlined } from '@ant-design/icons';
import { deleteObject, downloadObject, getPresignedUrl, getObjectUrl, headObject } from '../s3client';
import { formatBytes } from '../utils';
import type { S3Object } from '../types';
import { useColors } from '../ThemeContext';

interface Props {
  object: S3Object | null;
  onClose: () => void;
  onDeleted: () => void;
  isMobile?: boolean;
  headCache?: Record<string, { storageType?: string; storedSize?: number }>;
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

export default function InspectorPanel({ object, onClose, onDeleted, isMobile, headCache }: Props) {
  const {
    BG_SIDEBAR, BORDER, TEXT_PRIMARY, TEXT_MUTED, TEXT_FAINT,
    ACCENT_BLUE, ACCENT_GREEN, ACCENT_RED, STORAGE_TYPE_COLORS, STORAGE_TYPE_DEFAULT,
  } = useColors();
  const [messageApi, contextHolder] = message.useMessage();

  const [headData, setHeadData] = useState<{ headers: Record<string, string>; storageType?: string; storedSize?: number } | null>(null);
  const [headLoading, setHeadLoading] = useState(false);

  // Modal state for download / share operations (must be declared before early return)
  const [modalState, setModalState] = useState<
    | { mode: 'download'; phase: 'loading' | 'ready' | 'error'; error?: string }
    | { mode: 'share'; phase: 'loading' | 'ready' | 'error'; url?: string; error?: string }
    | null
  >(null);
  const blobRef = useRef<{ blob: Blob; name: string } | null>(null);

  useEffect(() => {
    if (!object) { setHeadData(null); return; }
    // Seed from table's headCache so Storage Stats renders instantly
    const cached = headCache?.[object.key];
    if (cached) {
      setHeadData({ headers: {}, storageType: cached.storageType, storedSize: cached.storedSize });
    } else {
      setHeadData(null);
    }
    setHeadLoading(true);
    headObject(object.key)
      .then(setHeadData)
      .catch(() => setHeadData((prev) => prev ?? { headers: {} }))
      .finally(() => setHeadLoading(false));
  }, [object?.key]);

  if (!object) return null;

  function Section({ title, children }: { title: string; children: React.ReactNode }) {
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

  function InfoRow({ label, value }: { label: string; value: string }) {
    return (
      <div style={{ padding: '8px 12px', background: BG_SIDEBAR, borderRadius: 8, marginBottom: 4 }}>
        <div style={{ fontSize: 11, color: TEXT_MUTED, marginBottom: 2, fontFamily: "var(--font-ui)", fontWeight: 500 }}>{label}</div>
        <div style={{ fontSize: 13, color: TEXT_PRIMARY, wordBreak: 'break-all', fontFamily: "var(--font-mono)" }}>{value}</div>
      </div>
    );
  }

  const fileName = object.key.split('/').pop() || object.key;
  const headers = headData?.headers ?? {};
  const storageType = headData?.storageType;
  const storedSize = headData?.storedSize;
  const rawSavings =
    storedSize != null && object.size > 0
      ? ((1 - storedSize / object.size) * 100)
      : 0;
  // Cap at 99.9% unless stored size is truly zero (avoid misleading "100.0%")
  const savings = rawSavings >= 100 && storedSize !== 0 ? 99.9 : rawSavings;
  const savedBytes = storedSize != null ? Math.max(0, object.size - storedSize) : 0;

  const dgMeta = getDgMetadata(headers);
  const userMeta = getUserMetadata(headers);

  const storageTypeLabel = storageType || 'Original';
  const storageTypeColor = STORAGE_TYPE_COLORS[storageType || 'passthrough'] || STORAGE_TYPE_DEFAULT;

  const handleDelete = async () => {
    try {
      await deleteObject(object.key);
      onClose();
      onDeleted();
    } catch {
      messageApi.error('Failed to delete object');
    }
  };

  const handleDownload = async () => {
    setModalState({ mode: 'download', phase: 'loading' });
    blobRef.current = null;
    try {
      const blob = await downloadObject(object.key);
      blobRef.current = { blob, name: fileName };
      setModalState({ mode: 'download', phase: 'ready' });
    } catch (e) {
      setModalState({ mode: 'download', phase: 'error', error: String(e) });
    }
  };

  const triggerBlobDownload = () => {
    if (!blobRef.current) return;
    const url = URL.createObjectURL(blobRef.current.blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = blobRef.current.name;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    setModalState(null);
  };

  const handleCopyLink = async () => {
    setModalState({ mode: 'share', phase: 'loading' });
    try {
      let url: string;
      try {
        url = await getPresignedUrl(object.key);
      } catch (e) {
        console.warn('Presigned URL failed, falling back to direct URL:', e);
        url = getObjectUrl(object.key);
      }
      setModalState({ mode: 'share', phase: 'ready', url });
    } catch (e) {
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
            {/* Download & Share buttons */}
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
              <Button
                type="primary"
                size="large"
                icon={<LinkOutlined />}
                onClick={handleCopyLink}
                style={{
                  flex: 1,
                  fontWeight: 600,
                  borderRadius: 10,
                  fontFamily: "var(--font-ui)",
                }}
              >
                Share
              </Button>
            </div>

            {/* STORAGE STATS */}
            <Section title="Storage Stats">
              {headLoading ? (
                <Skeleton active paragraph={{ rows: 4 }} />
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
                      <div style={{ fontSize: 12, fontWeight: 600, color: TEXT_PRIMARY, fontFamily: "var(--font-mono)" }}>{formatBytes(object.size)}</div>
                    </div>
                    <div style={{ height: 6, borderRadius: 3, background: `${TEXT_FAINT}33`, overflow: 'hidden' }}>
                      <div style={{ height: '100%', borderRadius: 3, width: '100%', background: `${TEXT_MUTED}66` }} />
                    </div>
                  </div>
                  <div style={{ marginBottom: 8 }}>
                    <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 4 }}>
                      <div style={{ fontSize: 11, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Stored</div>
                      <div style={{ fontSize: 12, fontWeight: 600, color: TEXT_PRIMARY, fontFamily: "var(--font-mono)" }}>
                        {storedSize != null ? formatBytes(storedSize) : formatBytes(object.size)}
                      </div>
                    </div>
                    <div style={{ height: 6, borderRadius: 3, background: `${TEXT_FAINT}33`, overflow: 'hidden' }}>
                      <div style={{
                        height: '100%',
                        borderRadius: 3,
                        width: `${storedSize != null && object.size > 0 ? Math.max(2, (storedSize / object.size) * 100) : 100}%`,
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
            </Section>

            {/* OBJECT INFO */}
            <Section title="Object Info">
              <InfoRow
                label="Last modified"
                value={object.lastModified ? new Date(object.lastModified).toLocaleString() : '--'}
              />
              <InfoRow label="Accept-Ranges" value="Disabled" />
            </Section>

            {/* S3 METADATA */}
            <Section title="S3 Metadata">
              {headLoading ? (
                <Skeleton active paragraph={{ rows: 1 }} />
              ) : (
                <InfoRow
                  label="Content-Type"
                  value={headers['content-type'] || 'binary/octet-stream'}
                />
              )}
            </Section>

            {/* CUSTOM METADATA (DG + User) */}
            <Section title="Custom Metadata">
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
            </Section>

            {/* TAGS */}
            <Section title="Tags">
              <div style={{ fontSize: 12, color: TEXT_FAINT, display: 'flex', alignItems: 'center', gap: 6, fontFamily: "var(--font-ui)" }}>
                No tags available
              </div>
            </Section>
          </div>

          {/* Delete button at bottom */}
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
              <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 16 }}>
                <Spin indicator={<LoadingOutlined style={{ fontSize: 40, color: ACCENT_GREEN }} />} />
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
              <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 16 }}>
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
              <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 16 }}>
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
              <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 16 }}>
                <Spin indicator={<LoadingOutlined style={{ fontSize: 40, color: ACCENT_BLUE }} />} />
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
              <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 16 }}>
                <CheckCircleFilled style={{ fontSize: 40, color: ACCENT_BLUE }} />
                <div style={{ width: '100%' }}>
                  <div style={{ fontSize: 16, fontWeight: 600, color: TEXT_PRIMARY, marginBottom: 12, fontFamily: "var(--font-ui)" }}>
                    Signed link ready
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
              <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 16 }}>
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
