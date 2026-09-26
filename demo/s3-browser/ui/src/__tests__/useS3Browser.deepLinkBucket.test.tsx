/**
 * Browser-review item 20: /_/browse/db-archive/?object=x from a session
 * whose last bucket was "releases" sent the inspector's HEAD to releases/x.
 * The s3client's module bucket was synced in a passive effect, and a child's
 * effect (the inspector's HEAD) runs before its parent's. The sync now runs
 * before any child effect, so the first request targets the URL's bucket.
 */
import { useEffect } from 'react';
import { expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';

const state = vi.hoisted(() => ({ bucket: 'releases' }));
vi.mock('../s3client', () => ({
  hasCredentials: () => true,
  getBucket: () => state.bucket,
  setBucket: (b: string) => { state.bucket = b; },
  headObject: async () => ({ storageType: 'passthrough', storedSize: 1 }),
  listObjects: async () => ({ objects: [], folders: [], isTruncated: false }),
}));

import useS3Browser from '../useS3Browser';
import { getBucket } from '../s3client';

test("a child's first effect sees the URL's bucket", () => {
  const seen: string[] = [];
  function Child() {
    useEffect(() => { seen.push(getBucket()); }, []);
    return null;
  }
  function Parent() {
    useS3Browser({ bucket: 'db-archive', prefix: '', q: '', object: 'x.zip', navigateUrl: () => {} });
    return <Child />;
  }
  renderWithQuery(<Parent />);
  expect(seen).toEqual(['db-archive']);
});
