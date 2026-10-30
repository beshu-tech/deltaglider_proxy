import { useState, useEffect, useId, useMemo, useRef } from 'react';
import { Layout, Button, Typography, Drawer, theme, message, Modal } from 'antd';
import Dropdown from './KeyboardDropdown';
import type { MenuProps } from 'antd';
import {
  PlusOutlined,
  DeleteOutlined,
  EditOutlined,
  EllipsisOutlined,
  UploadOutlined,
  ExclamationCircleOutlined,
  RightOutlined,
} from '@ant-design/icons';
import {
  countBucketObjects,
  deleteBucket,
  getBucket,
  invalidateListBucketsCache,
  listBuckets,
  setBucket,
} from '../s3client';
import type { BucketInfo } from '../types';
import { useColors } from '../ThemeContext';
import BucketEncryptionIcon from './BucketEncryptionIcon';
import MiddleEllipsis from '../browser/MiddleEllipsis';
import BucketPublicIcon from './BucketPublicIcon';
import SidebarResizeHandle from './SidebarResizeHandle';
import { describeBackend } from '../bucketBackend';
import {
  COLLAPSED_GROUPS_STORAGE_KEY,
  SIDEBAR_DEFAULT_WIDTH,
  groupBucketsByBackend,
  groupKey,
  parseCollapsedGroups,
  serializeCollapsedGroups,
  type BucketGroup,
} from '../sidebarLayout';
import { readStorage, writeStorage } from '../safeStorage';
import { useSidebarWidth } from '../useSidebarWidth';
import {
  BUCKET_PROBE_CAP,
  BUCKET_PROBE_TIMEOUT_MS,
  bucketDeleteRefusalText,
  classifyBucketDeleteRefusal,
  deleteConfirmText,
  deleteMenuEntry,
  type BucketObjectCount,
} from '../bucketDelete';
import { ApiError } from '../errorHandling';
import CreateBucketModal from './CreateBucketModal';
import { useNavigation } from '../NavigationContext';
import { bucketSettingsUrl, parseViewLocation } from '../urlState';

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
const LIST_STYLE: React.CSSProperties = { listStyle: 'none', margin: 0, padding: 0 };

interface Props {
  onUploadClick: () => void;
  onBucketChange: (bucket: string) => void;
  onBucketsChanged?: (count: number) => void;
  /** The listed bucket names, after every listing. When set, a linked
   *  bucket that is not in the list stays at its URL: the caller shows it
   *  as missing instead of the sidebar switching to another bucket. */
  onBucketNamesChanged?: (names: string[]) => void;
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
  onBucketNamesChanged,
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
  const [collapsedGroups, setCollapsedGroups] = useState(
    () => parseCollapsedGroups(readStorage(COLLAPSED_GROUPS_STORAGE_KEY)),
  );
  const sidebarWidth = useSidebarWidth();
  const groupIdPrefix = useId();
  const { token } = theme.useToken();
  const [messageApi, contextHolder] = message.useMessage();
  const { navigate } = useNavigation();

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
        onBucketNamesChanged?.(list.map((b) => b.name));
        const requested = getBucket();
        // A linked bucket that is not listed stays at its URL when the
        // caller shows it as missing; switching to another bucket in
        // silence made the link look like it worked.
        const callerShowsMissing = Boolean(requested && onBucketNamesChanged);
        if (list.length > 0 && !callerShowsMissing && !list.some((b) => b.name === requested)) {
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
  }, [onBucketChange, onBucketsChanged, onBucketNamesChanged, includeBucketOrigins, reloadSignal, messageApi]);

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
      onBucketNamesChanged?.(updated.map((b) => b.name));
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

  const refreshBucketsAfterDelete = async (name: string) => {
    const updated = await listBuckets({ includeOrigins: includeBucketOrigins });
    setBuckets(updated);
    onBucketsChanged?.(updated.length);
    onBucketNamesChanged?.(updated.map((b) => b.name));
    if (getBucket() === name && updated.length > 0) {
      setBucket(updated[0].name);
      onBucketChange(updated[0].name);
    } else if (getBucket() === name) {
      setBucket('');
      onBucketChange('');
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
      // The proxy removes the multipart uploads of a bucket with no object
      // itself, so the browser never aborts uploads: they can be other
      // users' uploads in progress (bucketDelete.ts).
      const refusal = classifyBucketDeleteRefusal(
        e instanceof ApiError ? e.code : undefined,
        e instanceof ApiError && e.detail ? e.detail : msg,
      );
      if (refusal) {
        messageApi.error(bucketDeleteRefusalText(name, refusal));
        return;
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

  // Settings need the admin GUI; delete needs admin on the bucket, and an
  // unavailable bucket offers only its settings (where its backend is fixed).
  const canEditSettings = canAdmin;
  const canDeleteRow = (b: BucketInfo) => !b.unavailable && canDeleteBucket(b.name);

  const bucketMenu = (b: BucketInfo): MenuProps => {
    const name = b.name;
    const entry = deleteMenuEntry(objectCounts[name]);
    const items: NonNullable<MenuProps['items']> = [];
    if (canEditSettings) {
      items.push({ key: 'settings', icon: <EditOutlined />, label: 'Edit settings' });
    }
    if (canDeleteRow(b)) {
      items.push({
        key: 'delete',
        danger: !entry.disabled,
        disabled: entry.disabled,
        icon: <DeleteOutlined />,
        label: entry.label,
        title: entry.disabled && objectCounts[name]?.state === 'known'
          ? 'Only an empty bucket can be deleted. Delete or move its objects first.'
          : undefined,
      });
    }
    return {
      items,
      onClick: ({ key, domEvent }) => {
        domEvent.stopPropagation();
        if (key === 'settings') navigate(bucketSettingsUrl(name));
        if (key === 'delete') confirmDeleteBucket(name);
      },
    };
  };

  const handleSelectBucket = (name: string) => {
    setBucket(name);
    onBucketChange(name);
  };

  const toggleGroup = (key: string) => {
    const next = new Set(collapsedGroups);
    if (!next.delete(key)) next.add(key);
    setCollapsedGroups(next);
    writeStorage(COLLAPSED_GROUPS_STORAGE_KEY, serializeCollapsedGroups(next));
  };

  const activeBucket = getBucket();
  // The upload page is a place in this sidebar: its entry is the current one
  // there. App re-renders the sidebar on every navigation, so the URL is
  // current here.
  const onUploadPage = parseViewLocation(window.location.pathname).view === 'upload';
  // Backend names come with the admin bucket-origins data only.
  const groups = useMemo(() => (canAdmin ? groupBucketsByBackend(buckets) : null), [buckets, canAdmin]);
  const menuItemStyle: React.CSSProperties = {
    gap: 10,
    padding: '8px 6px',
    color: TEXT_SECONDARY,
    fontSize: 13,
    width: '100%',
    transition: 'color 0.15s',
    fontFamily: "var(--font-ui)",
  };

  const renderBucketRow = (b: BucketInfo) => {
    const isUnavailable = Boolean(b.unavailable);
    const isActive = b.name === activeBucket;
    // On the upload page the bucket is the target, not the current entry:
    // it keeps its accent text and edge, the fill goes to "Upload files".
    const isCurrent = isActive && !onUploadPage;
    // The full name first: a long one is cut with an ellipsis in the row.
    const hoverText = [
      b.name,
      isUnavailable ? `Temporarily unavailable — ${b.unavailable}` : describeBackend(b.backend),
    ].filter(Boolean).join('\n');
    return (
      <li key={b.name} style={{ display: 'flex', alignItems: 'center', minWidth: 0 }}>
        <button
          className="btn-reset"
          onClick={() => { if (!isUnavailable) handleSelectBucket(b.name); }}
          disabled={isUnavailable}
          aria-current={isCurrent ? 'true' : undefined}
          aria-disabled={isUnavailable || undefined}
          title={hoverText}
          style={{
            flex: 1,
            minWidth: 0,
            gap: 8,
            padding: '7px 10px',
            borderRadius: 6,
            marginBottom: 2,
            cursor: isUnavailable ? 'not-allowed' : 'pointer',
            opacity: isUnavailable ? 0.5 : 1,
            background: isCurrent ? `rgba(45, 212, 191, 0.1)` : 'transparent',
            color: isActive ? ACCENT_BLUE_LIGHT : TEXT_SECONDARY,
            transition: 'all 0.15s ease',
            borderLeft: isActive ? `2px solid ${ACCENT_BLUE}` : '2px solid transparent',
          }}
          onMouseEnter={(e) => {
            if (!isUnavailable && !isCurrent) e.currentTarget.style.background = 'var(--surface-hover)';
          }}
          onMouseLeave={(e) => {
            if (!isCurrent) e.currentTarget.style.background = 'transparent';
          }}
        >
          {/* Cut in the middle: buckets often differ only at the end (-prod, -dr). */}
          <MiddleEllipsis
            text={b.name}
            title={false}
            style={{
              lineHeight: '20px',
              fontFamily: "var(--font-mono)",
              fontSize: 13,
              fontWeight: isActive ? 600 : 400,
            }}
          />
          <BucketPublicIcon bucket={b.name} canAdmin={canAdmin} />
          <BucketEncryptionIcon origin={b.backend} />
          {isUnavailable && (
            <span
              title={b.unavailable}
              style={{
                flexShrink: 0, marginLeft: 'auto', fontSize: 9, fontWeight: 700, letterSpacing: 0.5,
                textTransform: 'uppercase', padding: '1px 5px', borderRadius: 4,
                color: token.colorWarning,
                border: `1px solid ${token.colorWarning}`,
              }}
            >
              Unavailable
            </span>
          )}
        </button>
        {(canEditSettings || canDeleteRow(b)) && (
          <Dropdown
            menu={bucketMenu(b)}
            trigger={['click']}
            placement="bottomRight"
            onOpenChange={(isOpen) => { if (isOpen && canDeleteRow(b)) probeBucketContents(b.name); }}
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
  };

  const renderGroup = (group: BucketGroup, index: number) => {
    const key = groupKey(group);
    const expanded = !collapsedGroups.has(key);
    const label = group.backend ?? 'Unknown backend';
    // Not the backend name: an id must not hold spaces (aria-controls is a list).
    const listId = `${groupIdPrefix}-group-${index}`;
    // A collapsed group hides the open bucket: its header takes the accent
    // colour so the operator can see where the open bucket is.
    const hidesActive = !expanded && group.buckets.some((b) => b.name === activeBucket);
    // Backend-level facts only; the real bucket differs per row.
    const origin = group.buckets[0].backend;
    const backendFacts = describeBackend(origin && { ...origin, realBucket: undefined });
    return (
      <li key={`group:${key}`} style={{ marginTop: index === 0 ? 0 : 8 }}>
        <button
          className="btn-reset dg-bucket-group-header"
          aria-expanded={expanded}
          aria-controls={listId}
          aria-label={`${label} (${group.buckets.length} buckets)`}
          title={[backendFacts, hidesActive ? 'Contains the open bucket' : null].filter(Boolean).join('\n')}
          onClick={() => toggleGroup(key)}
          style={{
            '--dg-group-color': hidesActive ? ACCENT_BLUE_LIGHT : TEXT_MUTED,
            '--dg-group-hover-color': hidesActive ? ACCENT_BLUE_LIGHT : TEXT_PRIMARY,
          } as React.CSSProperties}
        >
          <RightOutlined aria-hidden="true" className="dg-bucket-group-chevron" />
          <span style={{
            flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
            fontFamily: 'var(--font-mono)', fontSize: 12, fontWeight: 600,
          }}>
            {label}
          </span>
          <span style={{ flexShrink: 0, fontSize: 11, color: TEXT_FAINT, fontVariantNumeric: 'tabular-nums' }}>
            {group.buckets.length}
          </span>
        </button>
        <ul id={listId} hidden={!expanded} style={{ ...LIST_STYLE, marginTop: 2 }}>
          {group.buckets.map(renderBucketRow)}
        </ul>
      </li>
    );
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

        {groups ? (
          <ul style={LIST_STYLE}>
            {groups.map(renderGroup)}
          </ul>
        ) : (
          <ul style={LIST_STYLE}>{buckets.map(renderBucketRow)}</ul>
        )}

        {canUpload && (
          <div style={{ padding: '4px 0', borderTop: `1px solid ${token.colorBorderSecondary}`, marginTop: 4 }}>
            <button
              className="btn-reset"
              onClick={onUploadClick}
              aria-current={onUploadPage ? 'page' : undefined}
              style={onUploadPage ? {
                ...menuItemStyle,
                borderRadius: 6,
                background: 'rgba(45, 212, 191, 0.1)',
                color: ACCENT_BLUE_LIGHT,
                fontWeight: 600,
                borderLeft: `2px solid ${ACCENT_BLUE}`,
              } : { ...menuItemStyle, borderLeft: '2px solid transparent' }}
              onMouseEnter={(e) => { if (!onUploadPage) e.currentTarget.style.color = TEXT_PRIMARY; }}
              onMouseLeave={(e) => { if (!onUploadPage) e.currentTarget.style.color = TEXT_SECONDARY; }}
            >
              <UploadOutlined aria-hidden="true" style={MENU_ICON_STYLE} />
              <span>Upload files</span>
            </button>
          </div>
        )}
      </nav>

      {/* Bottom group: glass panels + branding — pinned to bottom */}
      <div style={{ marginTop: 'auto' }}>
        {/* Product name and version: a tool sidebar, not a landing page. The
            build time is in the hover text. Version and build time come from
            the session-authenticated whoami — never from a build-time
            constant, which would bake the build identity into the public JS
            bundle. Blank until whoami resolves. */}
        <div
          style={{
            display: 'flex', alignItems: 'baseline', gap: 8,
            padding: '12px 16px', borderTop: `1px solid ${BORDER}`,
            fontSize: 12, fontFamily: 'var(--font-ui)', color: TEXT_MUTED,
          }}
          title={proxyBuildTime ? `Built ${formatBuildTime(proxyBuildTime)}` : undefined}
        >
          <span style={{ fontWeight: 700, color: TEXT_SECONDARY }}>DeltaGlider</span>
          {proxyVersion && <span style={{ fontVariantNumeric: 'tabular-nums' }}>v{proxyVersion}</span>}
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
          size={SIDEBAR_DEFAULT_WIDTH}
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
      width={sidebarWidth.width}
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
      <SidebarResizeHandle {...sidebarWidth} />
    </Sider>
  );
}
