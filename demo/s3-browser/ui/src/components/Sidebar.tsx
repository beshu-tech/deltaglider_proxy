import { useState, useEffect, useRef } from 'react';
import { Layout, Button, Typography, Drawer, Dropdown, theme, message, Modal } from 'antd';
import type { MenuProps } from 'antd';
import {
  PlusOutlined,
  DeleteOutlined,
  EllipsisOutlined,
  UploadOutlined,
  ExclamationCircleOutlined,
} from '@ant-design/icons';
import {
  abortAllMultipartUploads,
  countBucketObjects,
  countMultipartUploads,
  deleteBucket,
  getBucket,
  invalidateListBucketsCache,
  listBuckets,
  setBucket,
} from '../s3client';
import type { BucketInfo } from '../types';
import { useColors } from '../ThemeContext';
import BucketBackendBadge from './BucketBackendBadge';
import { showBackendChips } from '../bucketBackend';
import { BUCKET_PROBE_CAP, BUCKET_PROBE_TIMEOUT_MS, deleteConfirmText, deleteMenuEntry, type BucketObjectCount } from '../bucketDelete';
import CreateBucketModal from './CreateBucketModal';

const { Sider } = Layout;
const { Text } = Typography;

/** Format the server-reported ISO build timestamp for the sidebar footer. */
function formatBuildTime(iso: string): string {
  try {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' })
      + ' ' + d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  } catch {
    return iso;
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
  proxyBuildTime?: string;
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
  proxyBuildTime,
}: Props) {
  const {
    BG_SIDEBAR, BORDER, TEXT_PRIMARY, TEXT_SECONDARY,
    TEXT_MUTED, TEXT_FAINT, ACCENT_BLUE, ACCENT_BLUE_LIGHT,
  } = useColors();
  const [buckets, setBuckets] = useState<BucketInfo[]>([]);
  // Distinguishes "the bucket listing FAILED" (a backend is down / 503-ing)
  // from "there are genuinely zero buckets". Without this, a failed ListBuckets
  // collapses to an empty list that reads as "no buckets — create your first",
  // hiding an outage (the prod RCA: an upstream 503 emptied the sidebar with a
  // clean console). On error we keep whatever we last had and show a retry row.
  const [loadError, setLoadError] = useState<string | null>(null);
  const [reloadSignal, setReloadSignal] = useState(0);
  // Create-bucket dialog lives in <CreateBucketModal>; the Sidebar only owns
  // its open/close + post-create refresh of its local bucket list.
  const [createBucketOpen, setCreateBucketOpen] = useState(false);
  const [deletingBucketName, setDeletingBucketName] = useState<string | null>(null);
  const deleteConfirmOpenRef = useRef(false);
  const probeSeqRef = useRef<Record<string, number>>({});
  // Object count per bucket, probed when its row menu opens: Delete is only
  // offered for an empty bucket, because S3 refuses to delete any other.
  const [objectCounts, setObjectCounts] = useState<Record<string, BucketObjectCount>>({});
  const { token } = theme.useToken();
  const [messageApi, contextHolder] = message.useMessage();

  useEffect(() => {
    let cancelled = false;
    // A signal-driven reload is an explicit refresh (user click, post-upload):
    // bypass the short-lived shared listBuckets result so it can't look dead.
    if (reloadSignal > 0) invalidateListBucketsCache();
    listBuckets({ includeOrigins: includeBucketOrigins })
      .then((list) => {
        if (cancelled) return;
        setLoadError(null);
        setBuckets(list);
        onBucketsChanged?.(list.length);
        const requested = getBucket();
        if (list.length > 0 && !list.some((b) => b.name === requested)) {
          // Never auto-select an unavailable placeholder — it would drive the
          // object pane at a dead backend.
          const firstLive = list.find((b) => !b.unavailable) ?? list[0];
          // A deep link (or a bucket deleted elsewhere) used to swap silently
          // to another bucket; say why the page is not what was linked.
          if (requested) {
            messageApi.warning(
              `Bucket "${requested}" does not exist or you have no access to it — showing "${firstLive.name}".`,
              6,
            );
          }
          setBucket(firstLive.name);
          onBucketChange(firstLive.name);
        }
        if (list.length === 0 && getBucket()) {
          setBucket('');
          onBucketChange('');
        }
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        // Do NOT clear the list to [] — that would render the empty state and
        // mask the failure. Surface an error the operator can retry.
        setLoadError(formatError(e));
      });
    return () => {
      cancelled = true;
    };
  }, [onBucketChange, onBucketsChanged, includeBucketOrigins, reloadSignal, messageApi]);

  useEffect(() => {
    if (createBucketFocusSignal <= 0) return;
    setCreateBucketOpen(true);
  }, [createBucketFocusSignal]);

  // After CreateBucketModal reports success, refresh the Sidebar's local bucket
  // list (it's plain useState, not react-query) and open the new bucket — the
  // next thing anyone does after creating a bucket is put files in it. (It
  // used to switch only when no bucket was open, so the success toast left
  // the user in the old bucket.)
  const handleBucketCreated = async (name: string) => {
    try {
      const updated = await listBuckets({ includeOrigins: includeBucketOrigins });
      setBuckets(updated);
      onBucketsChanged?.(updated.length);
    } catch {
      /* refresh failure is non-fatal — the bucket was created */
    }
    setBucket(name);
    onBucketChange(name);
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
      content: deleteConfirmText(objectCounts[name]),
      okText: 'Delete',
      okButtonProps: { danger: true },
      cancelText: 'Cancel',
      onOk: () => handleDeleteBucket(name),
      afterClose: () => {
        deleteConfirmOpenRef.current = false;
      },
    });
  };

  const probeBucketContents = (name: string) => {
    // Only the newest probe per bucket may write: an older, slower answer
    // must not overwrite a newer one.
    const seq = (probeSeqRef.current[name] ?? 0) + 1;
    probeSeqRef.current[name] = seq;
    const settle = (value: BucketObjectCount) => {
      if (probeSeqRef.current[name] === seq) setObjectCounts((prev) => ({ ...prev, [name]: value }));
    };
    settle({ state: 'checking' });
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timeout = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error('timeout')), BUCKET_PROBE_TIMEOUT_MS);
    });
    Promise.race([countBucketObjects(name, BUCKET_PROBE_CAP), timeout])
      .then(({ count, truncated }) => settle({ state: 'known', count, truncated }))
      // A failed or hung probe must not block the operator: offer Delete, and
      // the server's answer (BucketNotEmpty and so on) still shows in a message.
      .catch(() => settle({ state: 'error' }))
      .finally(() => clearTimeout(timer));
  };

  const bucketMenu = (name: string): MenuProps => {
    const entry = deleteMenuEntry(objectCounts[name]);
    return {
      items: [
        {
          key: 'delete',
          danger: !entry.disabled,
          disabled: entry.disabled,
          icon: <DeleteOutlined />,
          label: entry.label,
          title: entry.disabled && objectCounts[name]?.state === 'known'
            ? 'Only an empty bucket can be deleted. Delete or move its objects first.'
            : undefined,
        },
      ],
      onClick: ({ key, domEvent }) => {
        domEvent.stopPropagation();
        if (key === 'delete') confirmDeleteBucket(name);
      },
    };
  };

  const handleSelectBucket = (name: string) => {
    setBucket(name);
    onBucketChange(name);
  };

  const activeBucket = getBucket();
  const backendChips = canAdmin && showBackendChips(buckets.map((b) => b.backend));
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

        {loadError && (
          <div
            role="alert"
            style={{
              margin: '0 0 10px', padding: '10px 12px', borderRadius: 6,
              border: `1px solid ${token.colorErrorBorder}`,
              background: token.colorErrorBg, fontSize: 12, lineHeight: 1.4,
            }}
          >
            <div style={{ color: token.colorError, fontWeight: 600, marginBottom: 4 }}>
              Couldn’t load buckets
            </div>
            <div style={{ color: TEXT_SECONDARY, marginBottom: 8, wordBreak: 'break-word' }}>
              A storage backend may be unavailable or rate-limiting requests.
              This is not the same as having no buckets — nothing has been created or deleted.
            </div>
            <div style={{ color: TEXT_SECONDARY, marginBottom: 8, wordBreak: 'break-word', opacity: 0.8 }}>
              {loadError}
            </div>
            <Button size="small" onClick={() => { setLoadError(null); setReloadSignal((n) => n + 1); }}>
              Retry
            </Button>
          </div>
        )}

        <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
          {buckets.map((b) => {
            const isUnavailable = Boolean(b.unavailable);
            return (
            <li key={b.name} style={{ display: 'flex', alignItems: 'center', minWidth: 0 }}>
              <button
                className="btn-reset"
                onClick={() => { if (!isUnavailable) handleSelectBucket(b.name); }}
                disabled={isUnavailable}
                aria-current={b.name === activeBucket ? 'true' : undefined}
                aria-disabled={isUnavailable || undefined}
                // Full origin backend error, verbatim, on hover — so an operator
                // sees exactly WHY the bucket is dark (e.g. the raw 503/SlowDown).
                title={isUnavailable ? `Temporarily unavailable — ${b.unavailable}` : undefined}
                style={{
                  flex: 1,
                  minWidth: 0,
                  padding: '7px 10px',
                  borderRadius: 6,
                  marginBottom: 2,
                  cursor: isUnavailable ? 'not-allowed' : 'pointer',
                  opacity: isUnavailable ? 0.5 : 1,
                  background: b.name === activeBucket ? `rgba(45, 212, 191, 0.1)` : 'transparent',
                  color: b.name === activeBucket ? ACCENT_BLUE_LIGHT : TEXT_SECONDARY,
                  transition: 'all 0.15s ease',
                  borderLeft: b.name === activeBucket ? `2px solid ${ACCENT_BLUE}` : '2px solid transparent',
                }}
                onMouseEnter={(e) => {
                  if (!isUnavailable && b.name !== activeBucket) e.currentTarget.style.background = 'var(--surface-hover)';
                }}
                onMouseLeave={(e) => {
                  if (b.name !== activeBucket) e.currentTarget.style.background = 'transparent';
                }}
              >
                <span style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0, overflow: 'hidden' }}>
                  <span style={{
                    fontFamily: "var(--font-mono)",
                    fontSize: 13,
                    fontWeight: b.name === activeBucket ? 600 : 400,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                    display: 'block',
                    // The name keeps its full width; the backend chip beside it
                    // gives way (and is clipped) first.
                    flex: 'none',
                    maxWidth: '100%',
                  }}>
                    {b.name}
                  </span>
                  {isUnavailable && (
                    <span
                      title={b.unavailable}
                      style={{
                        flexShrink: 0, fontSize: 9, fontWeight: 700, letterSpacing: 0.5,
                        textTransform: 'uppercase', padding: '1px 5px', borderRadius: 4,
                        color: token.colorWarning,
                        border: `1px solid ${token.colorWarning}`,
                      }}
                    >
                      Unavailable
                    </span>
                  )}
                  {backendChips && <BucketBackendBadge origin={b.backend} />}
                </span>
              </button>
              {!isUnavailable && canDeleteBucket(b.name) && (
                <Dropdown
                  menu={bucketMenu(b.name)}
                  trigger={['click']}
                  placement="bottomRight"
                  onOpenChange={(isOpen) => { if (isOpen) probeBucketContents(b.name); }}
                >
                  <Button
                    type="text"
                    size="small"
                    icon={<EllipsisOutlined />}
                    aria-label={`Actions for bucket ${b.name}`}
                    title="Bucket actions"
                    loading={deletingBucketName === b.name}
                    disabled={deletingBucketName !== null}
                    onClick={(e) => e.stopPropagation()}
                    style={{ color: TEXT_MUTED, flexShrink: 0 }}
                  />
                </Dropdown>
              )}
            </li>
            );
          })}
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
          {/* Version + build time come from the session-authenticated
              whoami — never from a build-time constant, which would bake
              the build identity into the public JS bundle. Blank until
              whoami resolves. */}
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
            {proxyVersion && (
              <span style={{ fontWeight: 400, letterSpacing: 0.5, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>
                v{proxyVersion}
              </span>
            )}
            {proxyBuildTime && <span>{formatBuildTime(proxyBuildTime)}</span>}
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
