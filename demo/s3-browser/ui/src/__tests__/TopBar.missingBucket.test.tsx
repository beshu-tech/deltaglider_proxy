/**
 * UX harvest D12: the missing-bucket screen kept a header that stated facts
 * about the bucket ("no-such-bucket · 0 B · 0 objects ⟳"). D51: on a phone
 * the header had no way to the upload page.
 */
import { screen } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import TopBar from '../components/TopBar';
import { renderWithQuery } from '../test/render';
import type { DeltaSummary } from '../deltaSummary';

const summary: DeltaSummary = {
  deltaCount: 3, referenceCount: 1, totalCount: 4, originalBytes: 1000, storedBytes: 700, referenceBytes: 0,
  savingsPct: 30, truncated: false, estimated: false, loading: false,
};

function bar(over: Partial<Parameters<typeof TopBar>[0]> = {}) {
  return renderWithQuery(
    <TopBar
      bucket="no-such-bucket"
      prefix=""
      onNavigate={() => {}}
      isMobile={false}
      onMenuClick={() => {}}
      onRefresh={() => {}}
      searchQuery=""
      onSearchChange={() => {}}
      refreshing={false}
      deltaSummary={summary}
      {...over}
    />,
  );
}

test('a missing bucket: no refresh and no savings in the header', () => {
  bar({ bucketMissing: true });
  expect(screen.queryByRole('button', { name: 'Refresh object list' })).toBeNull();
  expect(screen.queryByText(/smaller/)).toBeNull();
});

test('an existing bucket keeps them', () => {
  bar();
  expect(screen.getByRole('button', { name: 'Refresh object list' })).toBeInTheDocument();
});

test('a phone header has an upload button', () => {
  const onUpload = vi.fn();
  bar({ bucketMissing: false, isMobile: true, onUpload });
  screen.getByRole('button', { name: 'Upload files' }).click();
  expect(onUpload).toHaveBeenCalled();
});
