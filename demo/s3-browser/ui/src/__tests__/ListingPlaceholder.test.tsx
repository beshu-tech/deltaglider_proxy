// ListingPlaceholder: what an empty folder shows. A failed listing always
// outranks the friendly empty state (the beshu-b2 incident).
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { ComponentProps } from 'react';
import { expect, test, vi } from 'vitest';
import ListingPlaceholder from '../screens/ListingPlaceholder';
import { renderWithQuery } from '../test/render';

type Props = ComponentProps<typeof ListingPlaceholder>;

function mount(over: Partial<Props> = {}) {
  const props: Props = {
    bucket: 'releases',
    prefix: '',
    searchQuery: '',
    loading: false,
    error: null,
    onlyHiddenEntries: false,
    bucketCount: 1,
    canUpload: true,
    canCreateBucket: false,
    canAdmin: false,
    onReload: vi.fn(),
    onNavigate: vi.fn(),
    onUpload: vi.fn(),
    onCreateBucket: vi.fn(),
    ...over,
  };
  renderWithQuery(<ListingPlaceholder {...props} />);
  return props;
}

test('a failed listing shows the error with Retry, never "No objects yet"', async () => {
  const user = userEvent.setup();
  const props = mount({ error: 'NoSuchBucket: gone' });
  expect(screen.getByText("Bucket 'releases' was not found on its storage backend")).toBeInTheDocument();
  expect(screen.queryByText(/No objects yet/)).toBeNull();
  await user.click(screen.getByRole('button', { name: 'Retry' }));
  expect(props.onReload).toHaveBeenCalled();
});

test('the first load shows a spinner', () => {
  mount({ loading: true });
  expect(screen.getByText('Loading objects...')).toBeInTheDocument();
});

test('an empty sub-folder offers upload and the parent folder', async () => {
  const user = userEvent.setup();
  const props = mount({ prefix: 'builds/nightly/' });
  expect(screen.getByText(/Nothing is stored under releases\/builds\/nightly\//)).toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: 'Go to the parent folder' }));
  expect(props.onNavigate).toHaveBeenCalledWith('builds/');
  await user.click(screen.getByRole('button', { name: 'Upload files here' }));
  expect(props.onUpload).toHaveBeenCalled();
});

test('no buckets: create one, and the setup wizard for an admin', () => {
  mount({ bucketCount: 0, canCreateBucket: true, canAdmin: true });
  expect(screen.getByText('Create a bucket before uploading objects or generating demo data.')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Create a bucket' })).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'Run the setup wizard' })).toBeInTheDocument();
});

test('a search with no hits says so', () => {
  mount({ searchQuery: 'zzz', prefix: 'builds/' });
  expect(screen.getByText('No objects or folders in releases/builds/ match "zzz".')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Go to the parent folder' })).toBeNull();
});
