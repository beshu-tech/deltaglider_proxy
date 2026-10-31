import { useEffect, useRef, useState } from 'react';
import { Alert } from 'antd';
import type { WhoamiResponse } from '../adminApi';
import type { BrowserCapabilities } from '../browserCapabilities';
import BulkActionBar from '../components/BulkActionBar';
import DeleteJobsBanner from '../components/DeleteJobsBanner';
import ObjectTable, { type ObjectTableCursor, type ObjectTableFolderSizes } from '../components/ObjectTable';
import type { MaintenanceJobView } from '../maintenanceStatus';
import type { S3Object } from '../types';
import type { S3Browser } from '../useS3Browser';
import BucketUsageChip from '../components/BucketUsageChip';
import { BULK_BAR_RESERVE, HEADER_STATS_MIN_VIEWPORT } from '../browser/layout';
import { useIsNarrow } from '../useIsNarrow';
import BucketMaintenanceBanner from './BucketMaintenanceBanner';
import ListingPlaceholder from './ListingPlaceholder';
import { plainError } from '../plainError';

/** The active bucket's server-side jobs (GET /jobs/bucket/:bucket). */
interface BucketJobs {
  /** The job that holds the bucket (re-encrypt, migrate, …), or null. */
  active: MaintenanceJobView | null;
  /** Running delete jobs. */
  deletes: MaintenanceJobView[];
  /** The server's write gate is closed. */
  writesBlocked: boolean;
  /** Read the bucket status again (after a cancel). */
  refetch: () => void;
}

interface Props {
  s3: S3Browser;
  bucket: string;
  caps: BrowserCapabilities;
  identity: WhoamiResponse | null;
  jobs: BucketJobs;
  /** null until the sidebar has listed the buckets. */
  bucketCount: number | null;
  isMobile: boolean;
  folderSizes: ObjectTableFolderSizes;
  cursor: ObjectTableCursor;
  onPreview: (obj: S3Object) => void;
  onUpload: () => void;
  onCreateBucket: () => void;
  onSessionExpired: () => void;
}

/** The object browser: job banners, the bulk bar and the folder listing. */
export default function BrowserScreen({
  s3,
  bucket,
  caps,
  identity,
  jobs,
  bucketCount,
  isMobile,
  folderSizes,
  cursor,
  onPreview,
  onUpload,
  onCreateBucket,
  onSessionExpired,
}: Props) {
  const isEmpty = s3.objects.length === 0 && s3.folders.length === 0;
  const { canAdmin } = caps;
  const barShown = s3.selectedKeys.size > 0 || s3.bulkRunning;
  // On a narrow window the header has room for the savings chip only: the
  // bucket totals move to the listing's footer (TopBar hides them there).
  const statsInFooter = useIsNarrow(HEADER_STATS_MIN_VIEWPORT) && !isMobile;
  // The folder last seen with rows. An empty listing of THAT folder means
  // the user emptied it (a delete or a move), not a mistyped path.
  // Rows count as this folder's only from the second render at it: on the
  // first, they can still be the previous folder's (the listing resets after
  // the render), and a new bucket then read "is now empty" (UX harvest D13).
  const location = `${bucket}\u0000${s3.prefix}`;
  const [filledLocation, setFilledLocation] = useState<string | null>(null);
  const lastRenderedLocation = useRef<string | null>(null);
  useEffect(() => {
    const settled = lastRenderedLocation.current === location;
    lastRenderedLocation.current = location;
    if (!isEmpty && settled) setFilledLocation(location);
    // Every new listing (a new array) is a render that may settle the rows.
  }, [isEmpty, location, s3.objects, s3.folders]);
  const emptied = isEmpty && filledLocation === location;
  return (
    <>
      {jobs.active && <BucketMaintenanceBanner job={jobs.active} writesBlocked={jobs.writesBlocked} />}
      <DeleteJobsBanner
        jobs={jobs.deletes}
        // A job started without an IAM user (admin password, open access) says "admin".
        me={identity?.user?.name ?? (canAdmin ? 'admin' : null)}
        canCancel={(job) => canAdmin || (!!identity?.user && job.triggered_by === identity.user.name)}
        onChanged={jobs.refetch}
      />
      {/* A running bulk action keeps the bar (and its progress) on screen
          even when the selection empties: a delete's or move's batch reloads
          the listing, and a folder change clears the selection. */}
      {barShown && (
        <BulkActionBar
          selectedCount={s3.selectedKeys.size}
          onDelete={caps.bulk.delete ? s3.bulkDelete : undefined}
          onCopy={caps.bulk.copy ? s3.bulkCopy : undefined}
          onMove={caps.bulk.move ? s3.bulkMove : undefined}
          onDownloadZip={caps.bulk.zip ? s3.downloadZip : undefined}
          progress={s3.bulkProgress}
          onCancel={s3.cancelBulk}
          currentPrefix={s3.prefix}
          selectionKeys={s3.selectedKeys}
          onSessionExpired={onSessionExpired}
        />
      )}

      <div style={{ flex: 1, overflow: 'auto' }}>
        {/* With stale rows on hand, a failed refresh shows them under a
            warning banner instead of the error placeholder. */}
        {s3.error != null && !isEmpty && (
          <Alert
            type="warning"
            showIcon
            banner
            title="This list can be out of date: the last refresh failed"
            description={plainError(s3.error, 'The proxy could not list the objects.').text}
            style={{ borderRadius: 0 }}
          />
        )}
        {isEmpty ? (
          <ListingPlaceholder
            bucket={bucket}
            prefix={s3.prefix}
            searchQuery={s3.searchQuery}
            loading={s3.loading}
            error={s3.error}
            onlyHiddenEntries={!s3.searchQuery && !s3.showHidden && s3.allFolders.length > 0}
            emptied={emptied}
            bucketCount={bucketCount}
            canUpload={caps.canUpload}
            canCreateBucket={caps.canCreateBucket}
            canAdmin={canAdmin}
            onReload={s3.mutate}
            onNavigate={s3.navigate}
            onUpload={onUpload}
            onCreateBucket={onCreateBucket}
          />
        ) : (
          <ObjectTable
            listing={{
              objects: s3.objects,
              folders: s3.folders,
              prefix: s3.prefix,
              isTruncated: s3.isTruncated,
              restListedAt: s3.restListedAt,
              refreshing: s3.refreshing,
              headCache: s3.headCache,
              virtualFolders: s3.virtualFolders,
            }}
            selection={{
              inspected: s3.inspectorObject,
              selectedKeys: s3.selectedKeys,
              onToggleKey: s3.toggleKey,
              onToggleAll: s3.toggleAll,
            }}
            actions={{ onSelect: (obj) => s3.openInspector(obj.key), onNavigate: s3.navigate, onPreview }}
            folderSizes={folderSizes}
            cursor={cursor}
            isMobile={isMobile}
            reserveBottom={barShown ? BULK_BAR_RESERVE : 0}
            footerExtra={statsInFooter && bucket ? <BucketUsageChip bucket={bucket} canAdmin={canAdmin} inFolder={!!s3.prefix} /> : undefined}
          />
        )}
      </div>
    </>
  );
}
