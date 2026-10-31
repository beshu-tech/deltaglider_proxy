import { Button } from 'antd';
import { CloudUploadOutlined, FolderOpenOutlined, InboxOutlined, SearchOutlined } from '@ant-design/icons';
import DemoDataGenerator from '../components/DemoDataGenerator';
import { EmptyState, ErrorState, LoadingState } from '../components/StatePlaceholders';
import { useNavigation } from '../NavigationContext';
import { buildViewUrl } from '../urlState';
import { parentPrefix } from '../utils';
import { plainError } from '../plainError';
import { SPACE } from '../components/shared-styles';
import { useColors } from '../ThemeContext';

interface Props {
  bucket: string;
  prefix: string;
  searchQuery: string;
  loading: boolean;
  /** The failure of the last listing (any error value), or null. */
  error: unknown;
  /** Empty only because "Show system files" is off (the folder holds .dg/ etc.). */
  onlyHiddenEntries: boolean;
  /** The folder had rows a moment ago: the user emptied it (a delete or a move). */
  emptied?: boolean;
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
 * the first-load skeleton, a failed listing, or the empty state with the
 * next step. Actions follow the button taxonomy (PageActions): upload is the
 * primary, the other way out the secondary; demo data is an admin's small
 * extra under them, never the first offer in a real bucket (D56, D69).
 */
export default function ListingPlaceholder({
  bucket,
  prefix,
  searchQuery,
  loading,
  error,
  onlyHiddenEntries,
  emptied = false,
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
  const { TEXT_MUTED } = useColors();
  const hasNoBuckets = bucketCount === 0;
  const isRootBucketEmpty = (bucketCount ?? 0) > 0 && prefix === '' && !searchQuery && !loading;

  if (loading) return <LoadingState variant="list" rows={8} label="Loading objects" />;

  // An unrefreshed FAULT always outranks the friendly empty state: a failed
  // listing must never render as "No objects yet" (the beshu-b2 incident: a
  // bucket whose backend was unreachable looked innocently empty).
  if (error) {
    const plain = plainError(error, 'The proxy could not list the objects.');
    const raw = `${plain.details ?? ''} ${plain.text}`;
    const missingOnBackend = /NoSuchBucket|bucket does not exist/i.test(raw);
    return (
      <div style={{ maxWidth: 560, margin: '0 auto', padding: `${SPACE.xl}px ${SPACE.md}px` }}>
        <ErrorState error={error} what={`the objects of ${bucket}`} onRetry={onReload} />
        {missingOnBackend && (
          <p style={{ margin: 0, textAlign: 'center', fontSize: 12.5, lineHeight: 1.45, fontFamily: 'var(--font-ui)', color: TEXT_MUTED }}>
            The bucket is in this proxy&apos;s configuration, but not on the backend it routes to. Check the
            bucket&apos;s backend under Settings, Storage, Buckets, or create the bucket on that backend.
          </p>
        )}
      </div>
    );
  }

  const location = prefix ? `${bucket}/${prefix}` : bucket;
  const uploadHere = canUpload ? (
    <Button type="primary" icon={<CloudUploadOutlined />} onClick={onUpload}>
      Upload files here
    </Button>
  ) : undefined;
  const goUp = prefix ? (
    <Button onClick={() => onNavigate(parentPrefix(prefix))}>
      {parentPrefix(prefix) ? 'Go to the parent folder' : `Go to ${bucket}`}
    </Button>
  ) : undefined;

  if (searchQuery) {
    return (
      <EmptyState
        icon={<SearchOutlined />}
        title="No matches"
        hint={`No objects or folders in ${bucket}/${prefix} match "${searchQuery}".`}
      />
    );
  }
  if (hasNoBuckets) {
    return (
      <EmptyState
        icon={<InboxOutlined />}
        title="There are no buckets"
        hint="Create a bucket before uploading objects or generating demo data."
        action={canCreateBucket ? (
          <Button type="primary" onClick={onCreateBucket}>Create a bucket</Button>
        ) : undefined}
        // A fresh proxy: the wizard sets backend, admin and a bucket in one go.
        secondaryAction={canAdmin ? (
          <Button onClick={() => navigate(buildViewUrl('admin', 'setup'))}>Run the setup wizard</Button>
        ) : undefined}
      />
    );
  }
  if (onlyHiddenEntries) {
    return (
      <EmptyState
        icon={<FolderOpenOutlined />}
        title="Only system files"
        hint='This folder holds only hidden system files. Turn on "Show system files" in the account menu to see them.'
      />
    );
  }
  if (emptied) {
    return (
      <EmptyState icon={<FolderOpenOutlined />} title={`${location} is now empty.`} action={uploadHere} secondaryAction={goUp} />
    );
  }
  if (prefix) {
    // The proxy stores no folder markers: a folder exists only while it
    // holds files, so an empty listing here is as likely a typo in the path
    // as an empty folder.
    return (
      <EmptyState
        icon={<FolderOpenOutlined />}
        title="Nothing is stored here"
        hint={`Nothing is stored under ${bucket}/${prefix}. A folder exists only while it holds files, so check the path for a typo${canUpload ? ', or upload files to create it' : ''}.`}
        action={uploadHere}
        secondaryAction={goUp}
      />
    );
  }
  return (
    <div>
      <EmptyState
        icon={<InboxOutlined />}
        title="No objects yet"
        hint={canUpload ? `Upload files to ${bucket} to start.` : `${bucket} holds no objects.`}
        action={isRootBucketEmpty && canUpload ? (
          <Button type="primary" icon={<CloudUploadOutlined />} onClick={onUpload}>Upload files</Button>
        ) : undefined}
      />
      {/* Fixture objects inside a real bucket: an administrator's extra, last. */}
      {isRootBucketEmpty && canUpload && canAdmin && (
        <div style={{ display: 'flex', justifyContent: 'center', marginTop: -SPACE.md }}>
          <DemoDataGenerator onDone={onReload} label={`Add demo data to ${bucket}`} />
        </div>
      )}
    </div>
  );
}
