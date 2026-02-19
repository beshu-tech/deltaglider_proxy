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
        // Auto-select first bucket if current one doesn't exist in the list
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

  const sidebarContent = (
    <div className="dot-grid-bg" style={{ display: 'flex', flexDirection: 'column', height: '100%', background: BG_SIDEBAR }}>
      {contextHolder}

      {/* YOUR BUCKETS */}
      <nav aria-label="Bucket list" style={{ padding: '20px 16px 0', overflow: 'auto' }}>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
          <Text style={{ fontSize: 10, fontWeight: 700, letterSpacing: 1.5, textTransform: 'uppercase', color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
            Your Buckets ({buckets.length})
          </Text>
          <Tooltip title="Create bucket">
            <Button
              type="text"
              size="small"
              icon={<PlusOutlined />}
              aria-label="Create bucket"
              style={{ color: TEXT_MUTED, fontSize: 11 }}
              onClick={() => {
                newBucketInputRef.current?.focus();
              }}
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
                  padding: '6px 8px',
                  borderRadius: 6,
                  marginBottom: 1,
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
                  fontSize: 12,
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
                    style={{ opacity: 0.4, fontSize: 11, flexShrink: 0, transition: 'opacity 0.15s' }}
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
              style={{ background: 'var(--input-bg)', borderColor: BORDER, fontSize: 12, fontFamily: "var(--font-mono)" }}
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
            style={{
              gap: 10,
              padding: '7px 4px',
              color: TEXT_SECONDARY,
              fontSize: 12,
              width: '100%',
              transition: 'color 0.15s',
              fontFamily: "var(--font-ui)",
            }}
            onMouseEnter={(e) => { e.currentTarget.style.color = TEXT_PRIMARY; }}
            onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
          >
            <UploadOutlined aria-hidden="true" style={{ fontSize: 13, width: 20, textAlign: 'center', display: 'inline-flex', justifyContent: 'center' }} />
            <span>Upload Files</span>
          </button>
          <DemoDataGenerator onDone={onMutate} />
        </div>
      </nav>

      {/* Bottom group: navigation + branding + logout — pinned to bottom */}
      <div style={{ marginTop: 'auto' }}>
        {/* Navigation */}
        <div style={{ padding: '12px 16px', borderTop: `1px solid ${BORDER}` }}>
          <nav aria-label="Settings and help">
            <button
              className="btn-reset"
              onClick={onSettingsClick}
              style={{
                gap: 10,
                padding: '7px 4px',
                color: TEXT_SECONDARY,
                fontSize: 12,
                width: '100%',
                transition: 'color 0.15s',
                fontFamily: "var(--font-ui)",
              }}
              onMouseEnter={(e) => { e.currentTarget.style.color = TEXT_PRIMARY; }}
              onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
            >
              <SettingOutlined aria-hidden="true" style={{ fontSize: 13, width: 20, textAlign: 'center', display: 'inline-flex', justifyContent: 'center' }} />
              <span>Admin Settings</span>
            </button>
            <a
              href="https://github.com/beshu-tech/deltaglider"
              target="_blank"
              rel="noopener noreferrer"
              className="btn-reset"
              style={{
                gap: 10,
                padding: '7px 4px',
                color: TEXT_SECONDARY,
                fontSize: 12,
                width: '100%',
                textDecoration: 'none',
                transition: 'color 0.15s',
                fontFamily: "var(--font-ui)",
              }}
              onMouseEnter={(e) => { e.currentTarget.style.color = TEXT_PRIMARY; }}
              onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
            >
              <FileTextOutlined aria-hidden="true" style={{ fontSize: 13, width: 20, textAlign: 'center', display: 'inline-flex', justifyContent: 'center' }} />
              <span>Documentation</span>
            </a>
            {onLogout && (
              <button
                className="btn-reset"
                onClick={onLogout}
                style={{
                  gap: 10,
                  padding: '7px 4px',
                  color: TEXT_SECONDARY,
                  fontSize: 12,
                  width: '100%',
                  transition: 'color 0.15s',
                  fontFamily: "var(--font-ui)",
                }}
                onMouseEnter={(e) => { e.currentTarget.style.color = ACCENT_RED; }}
                onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
              >
                <LogoutOutlined aria-hidden="true" style={{ fontSize: 13, width: 20, textAlign: 'center', display: 'inline-flex', justifyContent: 'center' }} />
                <span>Logout</span>
              </button>
            )}
          </nav>
        </div>

        {/* Branding */}
        <div style={{ padding: '16px 16px 20px', borderTop: `1px solid ${BORDER}` }}>
          <div style={{ fontSize: 18, fontWeight: 800, letterSpacing: 3, color: TEXT_PRIMARY, lineHeight: 1.2, fontFamily: "var(--font-ui)" }}>
            DELTAGLIDER
          </div>
          <div style={{ fontSize: 12, fontWeight: 600, letterSpacing: 2.5, color: ACCENT_BLUE, textTransform: 'uppercase', marginTop: 3, fontFamily: "var(--font-mono)" }}>
            Proxy
          </div>
          <div style={{ fontSize: 10, color: TEXT_FAINT, marginTop: 6, fontFamily: "var(--font-mono)" }}>
            Built {formatBuildTime()}
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
