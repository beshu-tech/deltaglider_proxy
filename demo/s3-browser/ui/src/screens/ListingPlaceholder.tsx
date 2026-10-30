import { Alert, Button, Empty, Space, Spin } from 'antd';
import DemoDataGenerator from '../components/DemoDataGenerator';
import { useNavigation } from '../NavigationContext';
import { buildViewUrl } from '../urlState';
import { parentPrefix } from '../utils';

interface Props {
  bucket: string;
  prefix: string;
  searchQuery: string;
  loading: boolean;
  error: string | null;
  /** Empty only because "Show system files" is off (the folder holds .dg/ etc.). */
  onlyHiddenEntries: boolean;
  /** null until the sidebar has listed the buckets. */
  bucketCount: number | null;
  canUpload: boolean;
  canCreateBucket: boolean;
  canAdmin: boolean;
  /** Retry, and after demo data: list again. */
  onReload: () => void;
  onNavigate: (prefix: string) => void;
  onUpload: () => void;
  onCreateBucket: () => void;
}

/**
 * What the browser shows instead of the table when the folder has no rows:
 * the first-load spinner, a failed listing, or the empty state with the
 * next step (upload, parent folder, create a bucket, demo data).
 */
export default function ListingPlaceholder({
  bucket,
  prefix,
  searchQuery,
  loading,
  error,
  onlyHiddenEntries,
  bucketCount,
  canUpload,
  canCreateBucket,
  canAdmin,
  onReload,
  onNavigate,
  onUpload,
  onCreateBucket,
}: Props) {
  const { navigate } = useNavigation();
  const hasNoBuckets = bucketCount === 0;
  const isRootBucketEmpty = (bucketCount ?? 0) > 0 && prefix === '' && !searchQuery && !loading;

  if (loading) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', padding: '64px 0' }}>
        <Spin description="Loading objects..." />
      </div>
    );
  }

  // An unrefreshed FAULT always outranks the friendly empty state: a failed
  // listing must never render as "No objects yet — add demo data" (the
  // beshu-b2 incident: a bucket whose backend was unreachable looked
  // innocently empty).
  if (error) {
    return (
      <div style={{ maxWidth: 560, margin: '48px auto', padding: '0 16px' }}>
        <Alert
          type="error"
          showIcon
          title={
            /NoSuchBucket/i.test(error)
              ? `Bucket '${bucket}' was not found on its storage backend`
              : /ServiceUnavailable|unavailable|503/i.test(error)
                ? `Bucket '${bucket}' is currently unavailable`
                : 'Failed to load objects'
          }
          description={
            <Space orientation="vertical" size={8}>
              <span>{error}</span>
              {/NoSuchBucket/i.test(error) && (
                <span>
                  The bucket exists in this proxy's configuration but not on the
                  backend it routes to — check the bucket's backend under
                  Settings → Storage → Buckets, or create it on that backend.
                </span>
              )}
              <Button size="small" onClick={onReload}>
                Retry
              </Button>
            </Space>
          }
          style={{ borderRadius: 8 }}
        />
      </div>
    );
  }

  return (
    <Empty
      description={<span style={{ display: 'inline-block', maxWidth: 520 }}>{
        searchQuery
          ? `No results for "${searchQuery}"`
          : hasNoBuckets
            ? 'Create a bucket before uploading objects or generating demo data.'
            : onlyHiddenEntries
              ? 'This folder holds only hidden system files. Turn on "Show system files" in the account menu to see them.'
              : prefix
              // The proxy stores no folder markers: a folder exists
              // only while it holds files, so an empty listing here
              // is as likely a typo in the path as an empty folder.
              ? `Nothing is stored under ${bucket}/${prefix}. A folder exists only while it holds files, so check the path for a typo${canUpload ? ', or upload files to create it' : ''}.`
              : 'No objects yet. Upload files or generate demo data.'
      }</span>}
      style={{ padding: '64px 0' }}
    >
      {prefix && !searchQuery && (
        <Space size={16} wrap style={{ justifyContent: 'center' }}>
          {canUpload && (
            <Button type="primary" onClick={onUpload}>
              Upload files here
            </Button>
          )}
          <Button onClick={() => onNavigate(parentPrefix(prefix))}>
            {parentPrefix(prefix) ? 'Go to the parent folder' : `Go to ${bucket}`}
          </Button>
        </Space>
      )}
      {hasNoBuckets && (canCreateBucket || canAdmin) && (
        <Space size={16} wrap style={{ justifyContent: 'center' }}>
          {canCreateBucket && (
            <Button type="link" onClick={onCreateBucket} style={{ paddingInline: 0 }}>
              Create a bucket
            </Button>
          )}
          {/* A fresh proxy: the wizard sets backend, admin and a bucket in one go. */}
          {canAdmin && (
            <Button type="link" onClick={() => navigate(buildViewUrl('admin', 'setup'))} style={{ paddingInline: 0 }}>
              Run the setup wizard
            </Button>
          )}
        </Space>
      )}
      {isRootBucketEmpty && canUpload && (
        <Space orientation="vertical" size={4} align="center">
          <DemoDataGenerator
            onDone={onReload}
            variant="empty-state"
            label={`Add demo data to ${bucket}`}
          />
          <Button type="link" onClick={onUpload} style={{ paddingInline: 0 }}>
            Upload from computer
          </Button>
        </Space>
      )}
    </Empty>
  );
}
