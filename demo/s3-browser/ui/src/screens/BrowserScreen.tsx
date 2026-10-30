import { Alert } from 'antd';
import type { WhoamiResponse } from '../adminApi';
import type { BrowserCapabilities } from '../browserCapabilities';
import BulkActionBar from '../components/BulkActionBar';
import DeleteJobsBanner from '../components/DeleteJobsBanner';
import ObjectTable, { type ObjectTableCursor, type ObjectTableFolderSizes } from '../components/ObjectTable';
import type { MaintenanceJobView } from '../maintenanceStatus';
import type { S3Object } from '../types';
import type { S3Browser } from '../useS3Browser';
import BucketMaintenanceBanner from './BucketMaintenanceBanner';
import ListingPlaceholder from './ListingPlaceholder';

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
      {(s3.selectedKeys.size > 0 || s3.bulkRunning) && (
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
        {s3.error && !isEmpty && (
          <Alert
            type="warning"
            showIcon
            banner
            title="Showing a stale listing — the last refresh failed"
            description={s3.error}
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
          />
        )}
      </div>
    </>
  );
}
