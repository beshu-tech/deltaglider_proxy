import { useState, useEffect, useRef } from 'react';
import { Layout, Button, Typography, Input, Drawer, theme, message, Modal } from 'antd';
import type { InputRef } from 'antd';
import {
  PlusOutlined,
  DeleteOutlined,
  UploadOutlined,
  ExclamationCircleOutlined,
} from '@ant-design/icons';
import { listBuckets, createBucket, deleteBucket, getBucket, setBucket } from '../s3client';
import type { BucketInfo } from '../types';
import { useColors } from '../ThemeContext';

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
  const [newBucketName, setNewBucketName] = useState('');
  const [createBucketOpen, setCreateBucketOpen] = useState(false);
  const [creatingBucket, setCreatingBucket] = useState(false);
  const [deletingBucketName, setDeletingBucketName] = useState<string | null>(null);
  const newBucketInputRef = useRef<InputRef>(null);
  const deleteConfirmOpenRef = useRef(false);
  const { token } = theme.useToken();
  const [messageApi, contextHolder] = message.useMessage();

  useEffect(() => {
    listBuckets()
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
  }, [onBucketChange, onBucketsChanged]);

  useEffect(() => {
    if (createBucketFocusSignal <= 0) return;
    setCreateBucketOpen(true);
  }, [createBucketFocusSignal]);

  useEffect(() => {
    if (!createBucketOpen) return;
    const id = window.setTimeout(() => newBucketInputRef.current?.focus(), 80);
    return () => window.clearTimeout(id);
  }, [createBucketOpen]);

  const handleCreateBucket = async () => {
    const name = newBucketName.trim();
    if (!name) return;
    setCreatingBucket(true);
    try {
      await createBucket(name);
      setNewBucketName('');
      setCreateBucketOpen(false);
      messageApi.success(`Bucket "${name}" created`);
      const updated = await listBuckets();
      setBuckets(updated);
      onBucketsChanged?.(updated.length);
      if (!getBucket()) {
        setBucket(name);
        onBucketChange(name);
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      messageApi.error(`Failed to create bucket: ${msg}`);
    } finally {
      setCreatingBucket(false);
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

  const handleDeleteBucket = async (name: string) => {
    setDeletingBucketName(name);
    try {
      await deleteBucket(name);
      messageApi.success(`Bucket "${name}" deleted`);
      const updated = await listBuckets();
      setBuckets(updated);
      onBucketsChanged?.(updated.length);
      if (getBucket() === name && updated.length > 0) {
        setBucket(updated[0].name);
        onBucketChange(updated[0].name);
      } else if (getBucket() === name) {
        setBucket('');
        onBucketChange('');
      }
    } catch (e: unknown) {
      const msg = formatError(e);
      messageApi.error(`Failed to delete bucket: ${msg}`);
      throw e;
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
                <span style={{
                  fontFamily: "var(--font-mono)",
                  fontSize: 13,
                  fontWeight: b.name === activeBucket ? 600 : 400,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                  display: 'block',
                }}>
                  {b.name}
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
    <Modal
      title="Create bucket"
      open={createBucketOpen && canCreateBucket}
      okText="Create"
      onOk={handleCreateBucket}
      confirmLoading={creatingBucket}
      okButtonProps={{ disabled: !newBucketName.trim() || !canCreateBucket }}
      onCancel={() => {
        if (creatingBucket) return;
        setCreateBucketOpen(false);
        setNewBucketName('');
      }}
      destroyOnHidden
    >
      <Input
        ref={newBucketInputRef}
        placeholder="Bucket name"
        aria-label="Bucket name"
        value={newBucketName}
        onChange={(e) => setNewBucketName(e.target.value)}
        onPressEnter={handleCreateBucket}
        style={{ fontFamily: "var(--font-mono)" }}
      />
    </Modal>
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
