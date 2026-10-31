import { useLayoutEffect, useRef, useState } from 'react';
import { HomeFilled } from '@ant-design/icons';
import { prefixSegments } from '../utils';
import { useColors } from '../ThemeContext';
import { buildBrowserUrl } from '../urlState';
import { useBucketOrigins } from '../queries/backends';
import BucketBackendBadge from './BucketBackendBadge';
import BucketEncryptionIcon from './BucketEncryptionIcon';
import { showBackendChips } from '../bucketBackend';
import type { BucketBackendOrigin } from '../types';
import TruncatedName from './TruncatedName';
import { SPACE } from './shared-styles';
import { crumbSegments } from '../browser/crumbs';

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

/** The current folder keeps at least this many characters before anything
 *  else gives way (UX harvest D6). */
const CURRENT_MIN_CHARS = 16;
/** The bucket keeps at least this many characters once the middle is gone. */
const BUCKET_MIN_CHARS = 6;

/** The floor of a crumb's width: `chars` characters of its label (plus the separator). */
function floorWidth(label: string, chars: number, separator: boolean): string {
  const n = Math.min(Array.from(label).length, chars);
  return separator ? `calc(${n}ch + 20px)` : `${n}ch`;
}

/** The widest a bucket crumb gets; a longer name is cut there even with room to spare. */
const BUCKET_MAX_PX = 160;

/** Is the bucket or the current folder cut because the header lacks room?
 *  (A bucket name over BUCKET_MAX_PX is cut anyway: that does not count.) */
function priorityNameSqueezed(ol: Element): boolean {
  return Array.from(ol.querySelectorAll<HTMLElement>('[data-crumb-priority]')).some((li) => {
    const max = Number(li.dataset.crumbPriority) || Infinity;
    if (li.clientWidth >= max - 1) return false;
    return Array.from(li.querySelectorAll<HTMLElement>('span')).some((s) => s.scrollWidth > s.clientWidth + 1);
  });
}

const segmentBase: React.CSSProperties = {
  fontSize: 13,
  fontWeight: 500,
  whiteSpace: 'nowrap',
  // A name is cut in the middle by <TruncatedName>: the end stays visible.
  display: 'flex',
  minWidth: 0,
  fontFamily: "var(--font-ui)",
  textDecoration: 'none',
  background: 'none',
  border: 'none',
  padding: 0,
  cursor: 'pointer',
};

export default function Breadcrumb({ bucket, prefix, onNavigate, canAdmin = false, compact = false }: Props) {
  const { TEXT_PRIMARY, TEXT_SECONDARY, TEXT_FAINT, ACCENT_BLUE } = useColors();
  const separatorStyle: React.CSSProperties = { color: TEXT_FAINT, margin: `0 ${SPACE.xs}px`, fontSize: 12, flexShrink: 0, userSelect: 'none' };
  const allSegments = prefixSegments(prefix);
  // Priority when the header is narrow (D6): the current folder, then the
  // bucket, then the folders between them. The middle collapses to "…"
  // first (always on a phone, and on a wider screen as soon as the bucket or
  // the current folder would be cut); the bucket shrinks next; the current
  // folder keeps CURRENT_MIN_CHARS characters.
  const where = `${bucket}\u0000${prefix}`;
  const [collapsedAt, setCollapsedAt] = useState<{ where: string; width: number } | null>(null);
  const collapseMiddle = compact || collapsedAt?.where === where;
  const segments = crumbSegments(allSegments, collapseMiddle);
  const listRef = useRef<HTMLOListElement>(null);
  useLayoutEffect(() => {
    const ol = listRef.current;
    if (!ol || compact || allSegments.length < 2) return;
    const check = () => {
      if (collapsedAt?.where !== where) {
        if (ol.scrollWidth > ol.clientWidth + 1 || priorityNameSqueezed(ol)) setCollapsedAt({ where, width: ol.clientWidth });
      } else if (ol.clientWidth > collapsedAt.width + 48) {
        // Wider again: try the full path; the check above collapses it again if it still does not fit.
        setCollapsedAt(null);
      }
    };
    check();
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(check);
    observer.observe(ol);
    return () => observer.disconnect();
  }, [where, compact, allSegments.length, collapsedAt]);

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
    <nav aria-label="Breadcrumb" style={{ minWidth: 0, flex: '0 1 auto' }}>
      <ol ref={listRef} style={{ display: 'flex', alignItems: 'center', minWidth: 0, overflow: 'hidden', listStyle: 'none', margin: 0, padding: 0, fontSize: 13 }}>
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
        <li
          data-crumb-priority={prefix ? BUCKET_MAX_PX : ''}
          style={{
            display: 'flex',
            // The bucket gives way before the current folder (a larger shrink
            // weight), but keeps a few characters.
            flexShrink: 4,
            minWidth: prefix ? floorWidth(bucket, BUCKET_MIN_CHARS, false) : 0,
          }}
        >
          {prefix ? (
            <a
              href={hrefFor('')}
              onClick={onCrumbClick('')}
              style={{ ...segmentBase, color: TEXT_SECONDARY, maxWidth: BUCKET_MAX_PX, transition: 'color 0.15s' }}
            >
              <TruncatedName text={bucket} />
            </a>
          ) : (
            <span style={{ ...segmentBase, color: TEXT_PRIMARY, fontWeight: 600 }} aria-current="location">
              <TruncatedName text={bucket} />
            </span>
          )}
        </li>

        {!compact && canAdmin && bucket && activeOrigin && (
          <li style={{ display: 'inline-flex', alignItems: 'center', gap: SPACE.xs, marginLeft: SPACE.xs }}>
            <BucketEncryptionIcon origin={activeOrigin} />
            <BucketBackendBadge origin={activeOrigin} />
          </li>
        )}

        {/* Prefix segments */}
        {segments.map((seg, i) => {
          const isLast = i === segments.length - 1;
          return (
            <li
              key={seg.prefix}
              data-crumb-priority={isLast ? '' : undefined}
              style={{
                display: 'flex',
                alignItems: 'center',
                // The current folder never goes below its floor; a middle
                // folder keeps its natural width until the middle collapses.
                minWidth: isLast ? floorWidth(seg.label, CURRENT_MIN_CHARS, true) : undefined,
                flexShrink: isLast ? 1 : 0,
              }}
            >
              <span aria-hidden="true" style={separatorStyle}>&rsaquo;</span>
              {isLast ? (
                <span
                  style={{ ...segmentBase, color: TEXT_PRIMARY, fontWeight: 600 }}
                  aria-current="location"
                >
                  <TruncatedName text={seg.label} />
                </span>
              ) : (
                <a
                  href={hrefFor(seg.prefix)}
                  onClick={onCrumbClick(seg.prefix)}
                  aria-label={seg.collapsed ? 'Parent folder' : undefined}
                  style={{ ...segmentBase, color: TEXT_SECONDARY, maxWidth: 140, transition: 'color 0.15s' }}
                >
                  {seg.collapsed ? seg.label : <TruncatedName text={seg.label} />}
                </a>
              )}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
