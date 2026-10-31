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
  const props = mount({ error: new Error('List objects failed (404) [NoSuchBucket]: gone') });
  expect(screen.getByRole('alert')).toHaveTextContent('Could not load the objects of releases');
  expect(screen.getByText('The bucket does not exist.')).toBeInTheDocument();
  expect(screen.getByText(/not on the backend it routes to/)).toBeInTheDocument();
  expect(screen.queryByText(/No objects yet/)).toBeNull();
  await user.click(screen.getByRole('button', { name: /Retry/ }));
  expect(props.onReload).toHaveBeenCalled();
});

test('the first load shows a skeleton', () => {
  mount({ loading: true });
  expect(screen.getByRole('status', { name: 'Loading objects' })).toBeInTheDocument();
});

test('an empty sub-folder offers upload and the parent folder', async () => {
  const user = userEvent.setup();
  const props = mount({ prefix: 'builds/nightly/' });
  expect(screen.getByText(/Nothing is stored under releases\/builds\/nightly\//)).toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: 'Go to the parent folder' }));
  expect(props.onNavigate).toHaveBeenCalledWith('builds/');
  await user.click(screen.getByRole('button', { name: /Upload files here/ }));
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

// UX harvest C18: a folder the user just emptied is not a mistyped path.
test('a folder emptied by the user says so, without the typo hint', () => {
  mount({ prefix: 'qa-copied/', emptied: true });
  expect(screen.getByText('releases/qa-copied/ is now empty.')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: /Upload files here/ })).toHaveClass('ant-btn-primary');
  expect(screen.queryByText(/typo/)).toBeNull();
  expect(screen.getByRole('button', { name: 'Go to releases' })).toBeInTheDocument();
});

// UX harvest D56, D69: an empty bucket offers Upload as the primary button;
// demo data is an administrator's small extra, never offered to others.
test('an empty bucket: Upload is the primary; demo data only for an admin, after it', () => {
  mount();
  expect(screen.getByRole('button', { name: /Upload files/ })).toHaveClass('ant-btn-primary');
  expect(screen.queryByText(/demo data/i)).toBeNull();
});

test('an admin also sees the demo data action, below Upload', () => {
  mount({ canAdmin: true });
  const upload = screen.getByRole('button', { name: /Upload files/ });
  const demo = screen.getByRole('button', { name: /Add demo data to releases/ });
  expect(upload.compareDocumentPosition(demo) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
});
