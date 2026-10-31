/**
 * The upload outcome model (UX harvest D23, D24, D27, D31, D7, D44): one
 * state per file drives the banner, the rows and the counts. A batch with
 * failures never shows the green "Upload complete"; a failed row leads with
 * the plain sentence and keeps the wire text behind Details; Retry shows
 * only where it can help; a dropped or picked file with a staged name
 * replaces that row; the bucket usage refreshes when files land.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, expect, test, vi, beforeEach } from 'vitest';
import { renderWithQuery, testQueryClient } from '../test/render';
import { ApiError } from '../errorHandling';

const uploadObject = vi.hoisted(() => vi.fn(async (..._args: unknown[]): Promise<Record<string, string>> => ({})));
vi.mock('../s3client', () => ({
  getBucket: () => 'qa-quota',
  listCommonPrefixes: async () => [],
  uploadObject,
}));

import UploadPage from '../components/UploadPage';

beforeEach(() => {
  uploadObject.mockReset();
  // jsdom has no layout: give the virtual queue list a size so it renders rows.
  vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockReturnValue(520);
  vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(800);
});
afterEach(() => vi.restoreAllMocks());

const QUOTA_ERROR = new ApiError(
  'Upload object b.bin failed (403) [AccessDenied]: Bucket quota exceeded: 59 KiB used + 59 KiB upload > 100 KiB limit (request-id: 49e72a4e)',
  403,
  'AccessDenied',
  'Bucket quota exceeded: 59 KiB used + 59 KiB upload > 100 KiB limit',
);

const fileInput = (container: HTMLElement) =>
  container.querySelector<HTMLInputElement>('input[type="file"]:not([webkitdirectory])')!;

test('one success and two quota failures: a warning banner, plain reasons, Remove instead of Retry', async () => {
  uploadObject.mockImplementation(async (key: unknown) => {
    if (key === 'a.bin') return {};
    throw QUOTA_ERROR;
  });
  const user = userEvent.setup();
  renderWithQuery(
    <UploadPage
      prefix=""
      onBack={() => {}}
      onDone={() => {}}
      initialFiles={[new File(['a'], 'a.bin'), new File(['b'], 'b.bin'), new File(['c'], 'c.bin')]}
    />,
  );
  await user.click(await screen.findByRole('button', { name: /Upload 3 files/ }));

  expect(await screen.findByText('1 of 3 files uploaded')).toBeInTheDocument();
  expect(screen.queryByText('Upload complete')).toBeNull();
  expect(screen.getByText(/2 failed\. The list below says why\./)).toBeInTheDocument();

  const failed = screen.getByRole('listitem', { name: /^b\.bin — failed$/ });
  // The sentence a person can act on, not the wire text.
  expect(within(failed).getByRole('alert')).toHaveTextContent(/^This bucket is full: 59 KiB is used of 100 KiB/);
  expect(within(failed).getByRole('alert')).not.toHaveTextContent(/request-id|\(403\)|AccessDenied/);
  expect(within(failed).getByText('Details')).toBeInTheDocument();
  // A quota failure fails again: no Retry, but Remove (D31); no zero figures.
  expect(within(failed).queryByRole('button', { name: /Retry/ })).toBeNull();
  expect(within(failed).getByRole('button', { name: 'Remove b.bin from the list' })).toBeInTheDocument();
  expect(within(failed).queryByText(/Parts|Speed|0\.0%/)).toBeNull();
  expect(screen.queryByRole('button', { name: /Retry failed$/ })).toBeNull();

  await user.click(within(failed).getByRole('button', { name: 'Remove b.bin from the list' }));
  await waitFor(() => expect(screen.queryByRole('listitem', { name: /^b\.bin/ })).toBeNull());
});

test('a transient failure offers Retry, and a fully successful batch is the green "Upload complete"', async () => {
  let calls = 0;
  uploadObject.mockImplementation(async () => {
    calls += 1;
    if (calls === 1) throw new ApiError('Upload object x.bin failed (503) [SlowDown]: Please reduce your request rate.', 503, 'SlowDown');
    return {};
  });
  const user = userEvent.setup();
  renderWithQuery(<UploadPage prefix="" onBack={() => {}} onDone={() => {}} initialFiles={[new File(['x'], 'x.bin')]} />);
  await user.click(await screen.findByRole('button', { name: /Upload 1 file/ }));
  expect(await screen.findByText('No files were uploaded')).toBeInTheDocument();
  await user.click(screen.getByRole('button', { name: /Retry failed$/ }));
  expect(await screen.findByText('Upload complete')).toBeInTheDocument();
  expect(screen.getByText(/1 file uploaded to qa-quota\//)).toBeInTheDocument();
  expect(screen.getByRole('button', { name: /Done$/ })).toBeInTheDocument();
});

test('a picked file with a staged name replaces that row and says so (D27)', async () => {
  const user = userEvent.setup();
  const { container } = renderWithQuery(<UploadPage prefix="" onBack={() => {}} onDone={() => {}} />);
  await user.upload(fileInput(container), [new File(['12345678'], 'qa-dnd-1.txt', { lastModified: 1 })]);
  await user.upload(fileInput(container), [new File(['123'], 'qa-dnd-1.txt', { lastModified: 2 })]);
  const list = await screen.findByRole('list', { name: 'Staged files' });
  expect(within(list).getAllByRole('listitem')).toHaveLength(1);
  expect(within(list).getByText('3 B')).toBeInTheDocument();
  expect(within(list).getByText(/Replaces the file with the same name/)).toBeInTheDocument();
  expect(screen.getByText(/^1 file ready to upload/)).toBeInTheDocument();
});

test('the bucket usage (header counts and quota) refreshes after files land (D7)', async () => {
  uploadObject.mockResolvedValue({});
  const client = testQueryClient();
  const invalidate = vi.spyOn(client, 'invalidateQueries');
  const user = userEvent.setup();
  renderWithQuery(<UploadPage prefix="" onBack={() => {}} onDone={() => {}} initialFiles={[new File(['x'], 'x.bin')]} />, { client });
  await user.click(await screen.findByRole('button', { name: /Upload 1 file/ }));
  await screen.findByText('Upload complete');
  await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ['bucket-usage', 'qa-quota'] }));
});

test('a row in flight is a progressbar with a value (D44)', async () => {
  uploadObject.mockImplementation(() => new Promise(() => {}));
  const user = userEvent.setup();
  renderWithQuery(<UploadPage prefix="" onBack={() => {}} onDone={() => {}} initialFiles={[new File(['x'], 'x.bin')]} />);
  await user.click(await screen.findByRole('button', { name: /Upload 1 file/ }));
  const bar = await screen.findByRole('progressbar', { name: 'Upload progress of x.bin' });
  expect(bar).toHaveAttribute('aria-valuenow', '0');
  expect(screen.getByText(/Uploading: 0 of 1 files finished/)).toBeInTheDocument();
});
