import { useState, useEffect, useRef } from 'react';
import { Layout, Button, Space, Typography, Input, Drawer, theme, message, Popconfirm, Tooltip } from 'antd';
import type { InputRef } from 'antd';
import {
  SettingOutlined,
  FileTextOutlined,
  PlusOutlined,
  DeleteOutlined,
  UploadOutlined,
  LogoutOutlined,
} from '@ant-design/icons';
import { listBuckets, createBucket, deleteBucket, getBucket, setBucket } from '../s3client';
import type { BucketInfo } from '../types';
import DemoDataGenerator from './DemoDataGenerator';
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
  onMutate: () => void;
  refreshTrigger: number;
  onBucketChange: (bucket: string) => void;
  open: boolean;
  onClose: () => void;
  isMobile: boolean;
  onSettingsClick?: () => void;
  onLogout?: () => void;
}

export default function Sidebar({
  onUploadClick,
  onMutate,
  refreshTrigger,
  onBucketChange,
  open,
  onClose,
  isMobile,
  onSettingsClick,
  onLogout,
}: Props) {
  const {
    BG_SIDEBAR, BORDER, TEXT_PRIMARY, TEXT_SECONDARY,
    TEXT_MUTED, TEXT_FAINT, ACCENT_BLUE, ACCENT_BLUE_LIGHT, ACCENT_RED,
  } = useColors();
  const [buckets, setBuckets] = useState<BucketInfo[]>([]);
  const [newBucketName, setNewBucketName] = useState('');
  const [creatingBucket, setCreatingBucket] = useState(false);
  const newBucketInputRef = useRef<InputRef>(null);
  const { token } = theme.useToken();
  const [messageApi, contextHolder] = message.useMessage();

  useEffect(() => {
    listBuckets()
      .then((list) => {
        setBuckets(list);
        if (list.length > 0 && !list.some((b) => b.name === getBucket())) {
          setBucket(list[0].name);
          onBucketChange(list[0].name);
        }
      })
      .catch(() => setBuckets([]));
  }, [refreshTrigger]);

  const handleCreateBucket = async () => {
    const name = newBucketName.trim();
    if (!name) return;
    setCreatingBucket(true);
    try {
      await createBucket(name);
      setNewBucketName('');
      messageApi.success(`Bucket "${name}" created`);
      const updated = await listBuckets();
      setBuckets(updated);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      messageApi.error(`Failed to create bucket: ${msg}`);
    } finally {
      setCreatingBucket(false);
    }
  };

  const handleDeleteBucket = async (name: string) => {
    try {
      await deleteBucket(name);
      messageApi.success(`Bucket "${name}" deleted`);
      const updated = await listBuckets();
      setBuckets(updated);
      if (getBucket() === name && updated.length > 0) {
        setBucket(updated[0].name);
        onBucketChange(updated[0].name);
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : 'Unknown error';
      messageApi.error(`Failed to delete bucket: ${msg}`);
    }
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
      <nav aria-label="Bucket list" style={{ padding: '20px 16px 0', overflow: 'auto' }}>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 10 }}>
          <Text style={{ fontSize: 11, fontWeight: 700, letterSpacing: 1.5, textTransform: 'uppercase', color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
            Buckets ({buckets.length})
          </Text>
          <Tooltip title="Create bucket">
            <Button
              type="text"
              size="small"
              icon={<PlusOutlined />}
              aria-label="Create bucket"
              style={{ color: TEXT_MUTED, fontSize: 13 }}
              onClick={() => { newBucketInputRef.current?.focus(); }}
            />
          </Tooltip>
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
              {b.name !== activeBucket && (
                <Popconfirm
                  title={`Delete bucket "${b.name}"?`}
                  description="Bucket must be empty."
                  onConfirm={(e) => { e?.stopPropagation(); handleDeleteBucket(b.name); }}
                  onCancel={(e) => e?.stopPropagation()}
                  okText="Delete"
                  okType="danger"
                >
                  <Button
                    type="text"
                    size="small"
                    danger
                    icon={<DeleteOutlined />}
                    aria-label={`Delete bucket ${b.name}`}
                    onClick={(e) => e.stopPropagation()}
                    style={{ opacity: 0.4, fontSize: 12, flexShrink: 0, transition: 'opacity 0.15s' }}
                    onMouseEnter={(e) => { (e.currentTarget as HTMLElement).style.opacity = '1'; }}
                    onMouseLeave={(e) => { (e.currentTarget as HTMLElement).style.opacity = '0.4'; }}
                  />
                </Popconfirm>
              )}
            </li>
          ))}
        </ul>

        {/* New bucket input */}
        <div style={{ padding: '8px 0' }}>
          <Space.Compact style={{ width: '100%' }}>
            <Input
              ref={newBucketInputRef}
              size="small"
              placeholder="New bucket..."
              aria-label="New bucket name"
              value={newBucketName}
              onChange={(e) => setNewBucketName(e.target.value)}
              onPressEnter={handleCreateBucket}
              style={{ background: 'var(--input-bg)', borderColor: BORDER, fontSize: 13, fontFamily: "var(--font-mono)" }}
            />
            <Button
              size="small"
              icon={<PlusOutlined />}
              onClick={handleCreateBucket}
              loading={creatingBucket}
              aria-label="Create bucket"
            />
          </Space.Compact>
        </div>

        {/* Upload + Demo */}
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
          <DemoDataGenerator onDone={onMutate} />
        </div>
      </nav>

      {/* Bottom group: navigation + branding — pinned to bottom */}
      <div style={{ marginTop: 'auto' }}>
        {/* Navigation */}
        <div style={{ padding: '10px 16px 8px', borderTop: `1px solid ${BORDER}` }}>
          <nav aria-label="Settings and help">
            <button
              className="btn-reset"
              onClick={onSettingsClick}
              style={menuItemStyle}
              onMouseEnter={(e) => { e.currentTarget.style.color = TEXT_PRIMARY; }}
              onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
            >
              <SettingOutlined aria-hidden="true" style={MENU_ICON_STYLE} />
              <span>Admin Settings</span>
            </button>
            <a
              href="https://github.com/beshu-tech/deltaglider"
              target="_blank"
              rel="noopener noreferrer"
              className="btn-reset"
              style={{ ...menuItemStyle, textDecoration: 'none' }}
              onMouseEnter={(e) => { e.currentTarget.style.color = TEXT_PRIMARY; }}
              onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
            >
              <FileTextOutlined aria-hidden="true" style={MENU_ICON_STYLE} />
              <span>Documentation</span>
            </a>
            {onLogout && (
              <button
                className="btn-reset"
                onClick={onLogout}
                style={menuItemStyle}
                onMouseEnter={(e) => { e.currentTarget.style.color = ACCENT_RED; }}
                onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
              >
                <LogoutOutlined aria-hidden="true" style={MENU_ICON_STYLE} />
                <span>Logout</span>
              </button>
            )}
          </nav>
        </div>

        {/* Branding */}
        <div style={{ padding: '28px 16px 32px', borderTop: `1px solid ${BORDER}` }}>
          <div style={{ fontSize: 16, fontWeight: 800, letterSpacing: 4, color: TEXT_PRIMARY, lineHeight: 1, fontFamily: "var(--font-ui)", textTransform: 'uppercase' }}>
            DeltaGlider
          </div>
          <div style={{ fontSize: 11, fontWeight: 600, letterSpacing: 3, color: ACCENT_BLUE, textTransform: 'uppercase', marginTop: 5, fontFamily: "var(--font-mono)" }}>
            Proxy
          </div>
          <div style={{ fontSize: 10, color: TEXT_FAINT, marginTop: 14, fontFamily: "var(--font-mono)", letterSpacing: 0.3 }}>
            {formatBuildTime()}
          </div>
        </div>
      </div>{/* end bottom group */}
    </div>
  );

  if (isMobile) {
    return (
      <Drawer
        placement="left"
        size={260}
        open={open}
        onClose={onClose}
        styles={{ body: { padding: 0, background: BG_SIDEBAR } }}
      >
        {sidebarContent}
      </Drawer>
    );
  }

  return (
    <Sider
      width={250}
      style={{
        background: BG_SIDEBAR,
        borderRight: `1px solid ${BORDER}`,
        overflow: 'auto',
        height: '100vh',
        position: 'sticky',
        top: 0,
        left: 0,
      }}
    >
      <aside aria-label="Sidebar" style={{ height: '100%' }}>
        {sidebarContent}
      </aside>
    </Sider>
  );
}
