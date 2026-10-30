/**
 * What the file browser lets the user do here and now: one pure decision
 * from the identity, the session and where the user is. App and the screens
 * read the flags; nothing else re-derives them.
 */
import type { WhoamiResponse } from './adminApi';
import { canUse } from './permissions';
import type { SessionCapabilities } from './sessionCapabilities';

export interface BrowserCapabilityInput {
  identity: WhoamiResponse | null;
  session: SessionCapabilities;
  /** The active bucket ('' before one is chosen). */
  bucket: string;
  /** The folder the user is in. */
  prefix: string;
  /** A maintenance job holds the bucket's write gate. */
  bucketBusy: boolean;
  /** Writable folder roots in the bucket (`writablePrefixesForBucket`). */
  writablePrefixes: readonly string[];
  /** Checked rows: object keys and `folder:<prefix>` keys. */
  selectedKeys: Iterable<string>;
  /** The object open in the inspector, or null. */
  inspectedKey: string | null;
}

export interface BrowserCapabilities {
  /** Settings, setup wizard, any job: admin/open/bootstrap authority. */
  canAdmin: boolean;
  canCreateBucket: boolean;
  /** Refresh: read or list the current folder (true before a bucket). */
  canReadFolder: boolean;
  /** Write into the current folder (drop zone). */
  canWriteFolder: boolean;
  /** Open the upload page: the current folder or any writable prefix. */
  canUpload: boolean;
  /** The inspector's download and delete. */
  canReadInspected: boolean;
  canDeleteInspected: boolean;
  /** Bulk-bar actions: the permission AND a session that allows bulk requests. */
  bulk: { delete: boolean; copy: boolean; move: boolean; zip: boolean };
}

const FOLDER = 'folder:';

/** A selection key as a permission key: `folder:a/` → `a/`. */
function permissionKey(selectionKey: string): string {
  return selectionKey.startsWith(FOLDER) ? selectionKey.slice(FOLDER.length) : selectionKey;
}

export function deriveBrowserCapabilities(input: BrowserCapabilityInput): BrowserCapabilities {
  const { identity, session, bucket, prefix, bucketBusy, writablePrefixes, inspectedKey } = input;
  const selected = Array.from(input.selectedKeys, permissionKey);
  const hasBucket = Boolean(bucket);
  const every = (action: 'read' | 'delete') =>
    hasBucket && selected.length > 0 && selected.every((key) => canUse(identity, action, bucket, key));

  const canAdmin =
    identity?.mode === 'bootstrap' || identity?.mode === 'open' || identity?.user?.is_admin === true;
  const canWriteFolder = hasBucket && !bucketBusy && canUse(identity, 'write', bucket, prefix);
  // The upload page opens at the folder the user is in, even a read-only
  // one: it says so there and offers the writable prefixes.
  const canUpload = hasBucket && !bucketBusy && (canWriteFolder || writablePrefixes.length > 0);
  const canReadSelected = every('read');
  const canDeleteSelected = !bucketBusy && every('delete');
  const canCopySelected = canReadSelected && canWriteFolder;
  const canMoveSelected = canCopySelected && canDeleteSelected;
  const inspected = hasBucket && inspectedKey !== null;
  const bulkOk = session.canUseBulkActions;

  return {
    canAdmin,
    canCreateBucket: canUse(identity, 'admin'),
    canReadFolder: !hasBucket || canUse(identity, 'read', bucket, prefix) || canUse(identity, 'list', bucket, prefix),
    canWriteFolder,
    canUpload,
    canReadInspected: inspected && canUse(identity, 'read', bucket, inspectedKey ?? ''),
    canDeleteInspected: inspected && canUse(identity, 'delete', bucket, inspectedKey ?? ''),
    bulk: {
      delete: canDeleteSelected && bulkOk,
      copy: canCopySelected && bulkOk,
      move: canMoveSelected && bulkOk,
      zip: canReadSelected && bulkOk,
    },
  };
}
