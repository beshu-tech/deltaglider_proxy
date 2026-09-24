import { HomeFilled } from '@ant-design/icons';
import { prefixSegments } from '../utils';
import { useColors } from '../ThemeContext';
import { buildBrowserUrl } from '../urlState';
import { useBucketOrigins } from '../queries/backends';
import BucketBackendBadge from './BucketBackendBadge';
import { showBackendChips } from '../bucketBackend';
import type { BucketBackendOrigin } from '../types';

interface Props {
  /** The active bucket. Passed from the URL-derived source of truth — NOT read
   *  from s3client module state, which can lag the URL on a fresh load and
   *  produce wrong-bucket hrefs (middle-click / cmd-click would open the wrong
   *  bucket in a new tab). */
  bucket: string;
  prefix: string;
  onNavigate: (prefix: string) => void;
  /** Show the backend-origin chip. Gated to admins — the origins API
   *  (/api/admin/buckets) is admin-only. */
  canAdmin?: boolean;
  /** One-line mobile form: no Home icon and no backend chip; folders between
   *  the bucket and the current folder collapse into one "…" link to the
   *  parent folder. */
  compact?: boolean;
}

const segmentBase: React.CSSProperties = {
  fontSize: 13,
  fontWeight: 500,
  whiteSpace: 'nowrap',
  overflow: 'hidden',
  textOverflow: 'ellipsis',
  fontFamily: "var(--font-ui)",
  textDecoration: 'none',
  background: 'none',
  border: 'none',
  padding: 0,
  cursor: 'pointer',
};

export default function Breadcrumb({ bucket, prefix, onNavigate, canAdmin = false, compact = false }: Props) {
  const { TEXT_PRIMARY, TEXT_SECONDARY, TEXT_FAINT, ACCENT_BLUE } = useColors();
  const separatorStyle: React.CSSProperties = { color: TEXT_FAINT, margin: '0 6px', fontSize: 12, flexShrink: 0, userSelect: 'none' };
  const allSegments = prefixSegments(prefix);
  // Compact: "bucket › … › current". The "…" opens the parent folder.
  const segments: { label: string; prefix: string; collapsed?: boolean }[] = compact && allSegments.length > 1
    ? [{ ...allSegments[allSegments.length - 2], label: '…', collapsed: true }, allSegments[allSegments.length - 1]]
    : allSegments;

  // Backend-origin chip (admin-only — origins API is admin-gated). react-query
  // dedupes this with the Backends panel's useBucketOrigins(); `enabled` keeps
  // non-admins from firing the (403-ing) request.
  const originsQuery = useBucketOrigins({ enabled: canAdmin && !compact && Boolean(bucket) });
  const originRows = originsQuery.data?.buckets ?? [];
  const multiBackend = showBackendChips(originRows.map((b) => ({ backendName: b.backend_name || undefined })));
  const activeOrigin: BucketBackendOrigin | undefined = (() => {
    const row = originRows.find((b) => b.name === bucket);
    if (!row || !multiBackend) return undefined;
    return {
      backendName: row.backend_name || undefined,
      backendType: row.backend_type || undefined,
      backendEndpoint: row.backend_endpoint || undefined,
      backendRegion: row.backend_region || undefined,
      backendPath: row.backend_path || undefined,
      realBucket: row.real_bucket || undefined,
    };
  })();

  // Real <a href> so middle-click / cmd-click open the folder in a new tab.
  // Plain left-click is intercepted (preventDefault) and routed through the
  // SPA router via onNavigate; modified clicks fall through to the browser.
  const hrefFor = (p: string) => buildBrowserUrl({ bucket, prefix: p });
  const onCrumbClick = (p: string) => (e: React.MouseEvent) => {
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button !== 0) return;
    e.preventDefault();
    onNavigate(p);
  };

  return (
    <nav aria-label="Breadcrumb" style={{ minWidth: 0 }}>
      <ol style={{ display: 'flex', alignItems: 'center', minWidth: 0, overflow: 'hidden', listStyle: 'none', margin: 0, padding: 0 }}>
        {/* Home */}
        {!compact && <li>
          <a
            href={hrefFor('')}
            onClick={onCrumbClick('')}
            aria-label="Home"
            style={{ color: prefix ? TEXT_SECONDARY : ACCENT_BLUE, fontSize: 14, flexShrink: 0, transition: 'color 0.15s', textDecoration: 'none', cursor: 'pointer' }}
          >
            <HomeFilled aria-hidden="true" />
          </a>
        </li>}

        {!compact && <li aria-hidden="true" style={separatorStyle}>&rsaquo;</li>}

        {/* Bucket name */}
        <li style={{ display: 'flex', minWidth: 0, flexShrink: compact ? 1 : 0 }}>
          {prefix ? (
            <a
              href={hrefFor('')}
              onClick={onCrumbClick('')}
              style={{ ...segmentBase, color: TEXT_SECONDARY, maxWidth: 140, transition: 'color 0.15s' }}
            >
              {bucket}
            </a>
          ) : (
            <span style={{ ...segmentBase, color: TEXT_PRIMARY, maxWidth: 140, fontWeight: 600 }} aria-current="location">
              {bucket}
            </span>
          )}
        </li>

        {!compact && canAdmin && bucket && activeOrigin && (
          <li style={{ display: 'inline-flex', alignItems: 'center', marginLeft: 6 }}>
            <BucketBackendBadge origin={activeOrigin} />
          </li>
        )}

        {/* Prefix segments */}
        {segments.map((seg, i) => {
          const isLast = i === segments.length - 1;
          return (
            <li key={seg.prefix} style={{ display: 'flex', alignItems: 'center', minWidth: 0, flexShrink: isLast ? 1 : 0 }}>
              <span aria-hidden="true" style={separatorStyle}>&rsaquo;</span>
              {isLast ? (
                <span
                  style={{ ...segmentBase, color: TEXT_PRIMARY, fontWeight: 600 }}
                  aria-current="location"
                >
                  {seg.label}
                </span>
              ) : (
                <a
                  href={hrefFor(seg.prefix)}
                  onClick={onCrumbClick(seg.prefix)}
                  aria-label={seg.collapsed ? 'Parent folder' : undefined}
                  style={{ ...segmentBase, color: TEXT_SECONDARY, maxWidth: 140, transition: 'color 0.15s' }}
                >
                  {seg.label}
                </a>
              )}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
