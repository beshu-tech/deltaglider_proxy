import { useState, useEffect } from 'react';
import { Drawer, Button, message, Tag, Skeleton } from 'antd';
import { DownloadOutlined, DeleteOutlined, LinkOutlined, FileOutlined, CloseOutlined } from '@ant-design/icons';
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
    const blob = await downloadObject(object.key);
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = fileName;
    a.click();
    URL.revokeObjectURL(url);
  };

  const generateUrl = async (): Promise<string> => {
    try {
      return await getPresignedUrl(object.key);
    } catch (e) {
      console.warn('Presigned URL failed, falling back to direct URL:', e);
      return getObjectUrl(object.key);
    }
  };

  const handleCopyLink = async () => {
    try {
      const url = await generateUrl();
      await navigator.clipboard.writeText(url);
      messageApi.success('Link copied');
    } catch {
      messageApi.error('Failed to copy link');
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
    </>
  );
}
