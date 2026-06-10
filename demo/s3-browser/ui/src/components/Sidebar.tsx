import { useState, useEffect, useRef } from 'react';
import { Layout, Button, Typography, Drawer, theme, message, Modal } from 'antd';
import {
  PlusOutlined,
  DeleteOutlined,
  UploadOutlined,
  ExclamationCircleOutlined,
} from '@ant-design/icons';
import {
  abortAllMultipartUploads,
  countMultipartUploads,
  deleteBucket,
  getBucket,
  listBuckets,
  setBucket,
} from '../s3client';
import type { BucketInfo } from '../types';
import { useColors } from '../ThemeContext';
import BucketBackendBadge from './BucketBackendBadge';
import CreateBucketModal from './CreateBucketModal';

const { Sider } = Layout;
const { Text } = Typography;

/** Format the compile-time ISO timestamp into a human-readable string. */
function formatBuildTime(): string {
  try {
    const d = new Date(__BUILD_TIME__);
    return d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' })
      + ' ' + d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  } catch {
    return __BUILD_TIME__;
  }
}

/* Shared inline style constants for sidebar menu items */
const MENU_ICON_STYLE: React.CSSProperties = { fontSize: 14, width: 22, textAlign: 'center', display: 'inline-flex', justifyContent: 'center' };

interface Props {
  onUploadClick: () => void;
  onBucketChange: (bucket: string) => void;
  onBucketsChanged?: (count: number) => void;
  createBucketFocusSignal?: number;
  canCreateBucket: boolean;
  canDeleteBucket: (bucket: string) => boolean;
  canUpload: boolean;
  canAdmin: boolean;
  /** When false, skip loading extra bucket details from Settings (access-key-only sign-in). @default true */
  includeBucketOrigins?: boolean;
  open: boolean;
  onClose: () => void;
  isMobile: boolean;
  proxyVersion?: string;
}

export default function Sidebar({
  onUploadClick,
  onBucketChange,
  onBucketsChanged,
  createBucketFocusSignal = 0,
  canCreateBucket,
  canDeleteBucket,
  canUpload,
  canAdmin,
  includeBucketOrigins = true,
  open,
  onClose,
  isMobile,
  proxyVersion,
}: Props) {
  const {
    BG_SIDEBAR, BORDER, TEXT_PRIMARY, TEXT_SECONDARY,
    TEXT_MUTED, TEXT_FAINT, ACCENT_BLUE, ACCENT_BLUE_LIGHT,
  } = useColors();
  const [buckets, setBuckets] = useState<BucketInfo[]>([]);
  // Create-bucket dialog lives in <CreateBucketModal>; the Sidebar only owns
  // its open/close + post-create refresh of its local bucket list.
  const [createBucketOpen, setCreateBucketOpen] = useState(false);
  const [deletingBucketName, setDeletingBucketName] = useState<string | null>(null);
  const deleteConfirmOpenRef = useRef(false);
  const { token } = theme.useToken();
  const [messageApi, contextHolder] = message.useMessage();

  useEffect(() => {
    listBuckets({ includeOrigins: includeBucketOrigins })
      .then((list) => {
        setBuckets(list);
        onBucketsChanged?.(list.length);
        if (list.length > 0 && !list.some((b) => b.name === getBucket())) {
          setBucket(list[0].name);
          onBucketChange(list[0].name);
        }
        if (list.length === 0 && getBucket()) {
          setBucket('');
          onBucketChange('');
        }
      })
      .catch(() => {
        setBuckets([]);
        onBucketsChanged?.(0);
      });
  }, [onBucketChange, onBucketsChanged, includeBucketOrigins]);

  useEffect(() => {
    if (createBucketFocusSignal <= 0) return;
    setCreateBucketOpen(true);
  }, [createBucketFocusSignal]);

  // After CreateBucketModal reports success, refresh the Sidebar's local bucket
  // list (it's plain useState, not react-query) and auto-select the new bucket
  // if none is active yet.
  const handleBucketCreated = async (name: string) => {
    try {
      const updated = await listBuckets({ includeOrigins: includeBucketOrigins });
      setBuckets(updated);
      onBucketsChanged?.(updated.length);
    } catch {
      /* refresh failure is non-fatal — the bucket was created */
    }
    if (!getBucket()) {
      setBucket(name);
      onBucketChange(name);
    }
  };

  const formatError = (e: unknown): string => {
    if (e instanceof Error) {
      const named = e as Error & { Code?: unknown; code?: unknown };
      const code = typeof named.Code === 'string'
        ? named.Code
        : typeof named.code === 'string'
          ? named.code
          : '';
      return code && !e.message.includes(code) ? `${code}: ${e.message}` : e.message;
    }
    return typeof e === 'string' ? e : 'Unknown error';
  };

  const isBucketNotEmptyError = (messageText: string): boolean => /BucketNotEmpty/i.test(messageText);

  const parseMultipartCountFromError = (messageText: string): number | null => {
    const match = messageText.match(/multipart_uploads=(\d+)/i);
    if (!match) return null;
    const count = Number.parseInt(match[1], 10);
    return Number.isFinite(count) ? count : null;
  };

  const refreshBucketsAfterDelete = async (name: string) => {
    const updated = await listBuckets({ includeOrigins: includeBucketOrigins });
    setBuckets(updated);
    onBucketsChanged?.(updated.length);
    if (getBucket() === name && updated.length > 0) {
      setBucket(updated[0].name);
      onBucketChange(updated[0].name);
    } else if (getBucket() === name) {
      setBucket('');
      onBucketChange('');
    }
  };

  const abortMultipartUploadsAndRetryDelete = async (name: string, knownCount: number) => {
    setDeletingBucketName(name);
    try {
      const cleanup = await abortAllMultipartUploads(name);
      await deleteBucket(name);
      await refreshBucketsAfterDelete(name);
      messageApi.success(
        `Bucket "${name}" deleted after aborting ${cleanup.aborted || knownCount} multipart upload(s)`,
      );
      if (cleanup.remaining > 0) {
        messageApi.info(
          `${cleanup.remaining} upload(s) were still in-flight during cleanup and may need another attempt.`,
        );
      }
    } catch (e: unknown) {
      const msg = formatError(e);
      messageApi.error(`Cleanup + delete failed: ${msg}`);
    } finally {
      setDeletingBucketName(null);
    }
  };

  const handleDeleteBucket = async (name: string) => {
    setDeletingBucketName(name);
    try {
      await deleteBucket(name);
      messageApi.success(`Bucket "${name}" deleted`);
      await refreshBucketsAfterDelete(name);
    } catch (e: unknown) {
      const msg = formatError(e);
      if (isBucketNotEmptyError(msg)) {
        // Prefer blocker count from server error; if absent, derive via ListMultipartUploads.
        let mpuCount = parseMultipartCountFromError(msg) ?? 0;
        if (mpuCount <= 0) {
          try {
            mpuCount = await countMultipartUploads(name);
          } catch {
            mpuCount = 0;
          }
        }
        if (mpuCount > 0) {
          Modal.confirm({
            title: `Delete blocked by ${mpuCount} multipart upload(s)`,
            icon: <ExclamationCircleOutlined />,
            content:
              'This bucket looks empty but still has pending multipart uploads. Abort them and retry delete?',
            okText: 'Abort uploads and delete',
            okButtonProps: { danger: true },
            cancelText: 'Cancel',
            onOk: () => abortMultipartUploadsAndRetryDelete(name, mpuCount),
          });
          return;
        }
      }
      messageApi.error(`Failed to delete bucket: ${msg}`);
    }
    finally {
      setDeletingBucketName(null);
    }
  };

  const confirmDeleteBucket = (name: string) => {
    if (deleteConfirmOpenRef.current || deletingBucketName) return;
    deleteConfirmOpenRef.current = true;

    Modal.confirm({
      title: `Delete bucket "${name}"?`,
      icon: <ExclamationCircleOutlined />,
      content: 'Bucket must be empty. This removes the bucket itself.',
      okText: 'Delete',
      okButtonProps: { danger: true },
      cancelText: 'Cancel',
      onOk: () => handleDeleteBucket(name),
      afterClose: () => {
        deleteConfirmOpenRef.current = false;
      },
    });
  };

  const handleSelectBucket = (name: string) => {
    setBucket(name);
    onBucketChange(name);
  };

  const activeBucket = getBucket();
  const menuItemStyle: React.CSSProperties = {
    gap: 10,
    padding: '8px 6px',
    color: TEXT_SECONDARY,
    fontSize: 13,
    width: '100%',
    transition: 'color 0.15s',
    fontFamily: "var(--font-ui)",
  };

  const sidebarContent = (
    <div className="dot-grid-bg" style={{ display: 'flex', flexDirection: 'column', height: '100%', background: BG_SIDEBAR }}>
      {contextHolder}

      {/* BUCKETS */}
      <nav
        aria-label="Bucket list"
        style={{ flex: 1, minHeight: 0, overflow: 'auto', padding: '20px 16px 0' }}
      >
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 10 }}>
          <Text style={{ fontSize: 11, fontWeight: 700, letterSpacing: 1.5, textTransform: 'uppercase', color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
            Buckets ({buckets.length})
          </Text>
          {canCreateBucket && (
            <Button
              type="text"
              size="small"
              icon={<PlusOutlined />}
              aria-label="Create bucket"
              title="Create bucket"
              style={{ color: TEXT_MUTED, fontSize: 13 }}
              onClick={() => setCreateBucketOpen(true)}
            />
          )}
        </div>

        <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
          {buckets.map((b) => (
            <li key={b.name} style={{ display: 'flex', alignItems: 'center', minWidth: 0 }}>
              <button
                className="btn-reset"
                onClick={() => handleSelectBucket(b.name)}
                aria-current={b.name === activeBucket ? 'true' : undefined}
                style={{
                  flex: 1,
                  minWidth: 0,
                  padding: '7px 10px',
                  borderRadius: 6,
                  marginBottom: 2,
                  background: b.name === activeBucket ? `rgba(45, 212, 191, 0.1)` : 'transparent',
                  color: b.name === activeBucket ? ACCENT_BLUE_LIGHT : TEXT_SECONDARY,
                  transition: 'all 0.15s ease',
                  borderLeft: b.name === activeBucket ? `2px solid ${ACCENT_BLUE}` : '2px solid transparent',
                }}
                onMouseEnter={(e) => {
                  if (b.name !== activeBucket) e.currentTarget.style.background = 'var(--surface-hover)';
                }}
                onMouseLeave={(e) => {
                  if (b.name !== activeBucket) e.currentTarget.style.background = 'transparent';
                }}
              >
                <span style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
                  <span style={{
                    fontFamily: "var(--font-mono)",
                    fontSize: 13,
                    fontWeight: b.name === activeBucket ? 600 : 400,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                    display: 'block',
                    minWidth: 0,
                    flex: 1,
                  }}>
                    {b.name}
                  </span>
                  {canAdmin && <BucketBackendBadge origin={b.backend} />}
                </span>
              </button>
              {canDeleteBucket(b.name) && (
                <Button
                  type="text"
                  size="small"
                  danger
                  icon={<DeleteOutlined />}
                  aria-label={`Delete bucket ${b.name}`}
                  title={`Delete bucket ${b.name}`}
                  loading={deletingBucketName === b.name}
                  disabled={deletingBucketName !== null}
                  onClick={(e) => {
                    e.stopPropagation();
                    confirmDeleteBucket(b.name);
                  }}
                  style={{ opacity: b.name === activeBucket ? 0.75 : 0.4, fontSize: 12, flexShrink: 0, transition: 'opacity 0.15s' }}
                  onMouseEnter={(e) => { (e.currentTarget as HTMLElement).style.opacity = '1'; }}
                  onMouseLeave={(e) => { (e.currentTarget as HTMLElement).style.opacity = b.name === activeBucket ? '0.75' : '0.4'; }}
                />
              )}
            </li>
          ))}
        </ul>

        {canUpload && (
          <div style={{ padding: '4px 0', borderTop: `1px solid ${token.colorBorderSecondary}`, marginTop: 4 }}>
            <button
              className="btn-reset"
              onClick={onUploadClick}
              style={menuItemStyle}
              onMouseEnter={(e) => { e.currentTarget.style.color = TEXT_PRIMARY; }}
              onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
            >
              <UploadOutlined aria-hidden="true" style={MENU_ICON_STYLE} />
              <span>Upload Files</span>
            </button>
          </div>
        )}
      </nav>

      {/* Bottom group: glass panels + branding — pinned to bottom */}
      <div style={{ marginTop: 'auto' }}>
        {/* Branding */}
        <div style={{ padding: '28px 16px 32px', borderTop: `1px solid ${BORDER}` }}>
          <div style={{ fontSize: 16, fontWeight: 800, letterSpacing: 4, color: TEXT_PRIMARY, lineHeight: 1, fontFamily: "var(--font-ui)", textTransform: 'uppercase' }}>
            DeltaGlider
          </div>
          {/* Tagline on its own row so the version does not steal width (avoids awkward wraps). */}
          <div
            style={{
              fontSize: 10,
              fontWeight: 600,
              letterSpacing: 1.1,
              color: ACCENT_BLUE,
              textTransform: 'uppercase',
              marginTop: 6,
              fontFamily: "var(--font-ui)",
              lineHeight: 1.35,
            }}
          >
            Object storage control plane
          </div>
          {/* Prefer the server-reported version (whoami) since it
              reflects the actual running Rust binary; fall back to
              the build-time constant (read from Cargo.toml by Vite)
              so the sidebar still shows SOMETHING before whoami
              resolves — or if the API has diverged in a way that
              drops the field. The two should always agree in a
              healthy deployment. */}
          <div
            style={{
              display: 'flex',
              flexWrap: 'wrap',
              alignItems: 'baseline',
              gap: '4px 10px',
              marginTop: 10,
              fontSize: 10,
              color: TEXT_FAINT,
              fontFamily: "var(--font-mono)",
              letterSpacing: 0.3,
            }}
          >
            <span style={{ fontWeight: 400, letterSpacing: 0.5, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
              v{proxyVersion || __BUILD_VERSION__}
            </span>
            <span>{formatBuildTime()}</span>
          </div>
        </div>
      </div>{/* end bottom group */}
    </div>
  );

  const createBucketModal = (
    <CreateBucketModal
      open={createBucketOpen && canCreateBucket}
      canAdmin={canAdmin}
      onClose={() => setCreateBucketOpen(false)}
      onCreated={handleBucketCreated}
    />
  );

  if (isMobile) {
    return (
      <>
        <Drawer
          placement="left"
          size={260}
          open={open}
          onClose={onClose}
          styles={{ body: { padding: 0, background: BG_SIDEBAR } }}
        >
          {sidebarContent}
        </Drawer>
        {createBucketModal}
      </>
    );
  }

  return (
    <Sider
      width={250}
      style={{
        background: BG_SIDEBAR,
        borderRight: `1px solid ${BORDER}`,
        overflow: 'hidden',
        height: '100vh',
        position: 'sticky',
        top: 0,
        left: 0,
      }}
    >
      <aside aria-label="Sidebar" style={{ height: '100%' }}>
        {sidebarContent}
        {createBucketModal}
      </aside>
    </Sider>
  );
}
