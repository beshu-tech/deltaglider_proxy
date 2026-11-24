/**
 * The Storage card, the admin banner, the plain upload refusals and the
 * pre-check limit: the copy of each state is a contract (issue #204).
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, describe, expect, test, vi } from 'vitest';
import StorageCard from '../components/StorageCard';
import StorageBanner from '../components/StorageBanner';
import { useUploadLimits } from '../queries/uploadLimits';
import { queryClient } from '../queries/client';
import { qk } from '../queries/keys';
import { ApiError, normalizeS3Error, throwApiError } from '../errorHandling';
import { plainError } from '../plainError';
import { precheckUpload } from '../uploadPrecheck';
import { json, mockFetch } from '../test/fetchMock';
import { renderHookWithQuery, renderWithQuery } from '../test/render';

const GB = 1024 ** 3;
const STATUS = '/_/api/admin/storage/status';
afterEach(() => {
  vi.unstubAllGlobals();
  queryClient.clear();
});

const disk = { path: '/var/lib/dgp', free_bytes: 41 * GB, total_bytes: 500 * GB };
const cases: [string, object, string[]][] = [
  ['healthy', { state: 'healthy', largest_file: 37 * GB }, ['Storage is healthy', 'You can upload files up to 37 GiB.', 'Change the limit']],
  ['checking', { state: 'checking', largest_file: 37 * GB }, ['Checking the disk', 'You can upload files up to 37 GiB.']],
  [
    'filling_up',
    { state: 'filling_up', largest_file: 37 * GB, disk, measured_secs_ago: 3 },
    ['The disk is filling up', 'Uploads will pause when /var/lib/dgp is full. It has 41 GiB left.', 'Free up space on that disk.'],
  ],
  [
    'large_paused',
    { state: 'large_paused', largest_file: 37 * GB, disk, measured_secs_ago: 3 },
    ['Large uploads are paused', 'The disk /var/lib/dgp is full. Small files may still upload. Downloads still work.', 'Uploads resume by themselves when space is free.'],
  ],
  [
    'large_paused without a disk',
    { state: 'large_paused', largest_file: 37 * GB, disk: null, measured_secs_ago: null },
    ['Large uploads are paused', 'The disk is full. Small files may still upload. Downloads still work.', 'Uploads resume by themselves when space is free.'],
  ],
  [
    'backend_paused',
    {
      state: 'backend_paused',
      largest_file: 37 * GB,
      backends: [{ name: 'hetzner-fsn1', path: '/mnt/a', free_bytes: 0 }, { name: 'local-disk', path: '/mnt/b', free_bytes: 0 }],
      large_uploads_paused: true,
      measured_secs_ago: 3,
    },
    [
      'Uploads are paused',
      'The disk /mnt/a is full. Uploads to its buckets are paused. Downloads still work.',
      'The disk /mnt/b is full. Uploads to its buckets are paused. Downloads still work.',
      'Uploads resume by themselves when space is free.',
    ],
  ],
];

describe('the Storage card', () => {
  test.each(cases)('%s shows exactly its copy', async (_name, body, copy) => {
    mockFetch().on('GET', STATUS, json(body));
    renderWithQuery(<StorageCard />);
    await screen.findByText(copy[0]);
    const card = screen.getByTestId('storage-card');
    // Title, then every sentence, whole.
    for (const text of copy) expect(card).toHaveTextContent(text);
    const expected = copy.join('');
    expect((card.textContent ?? '').replace(/\s+/g, '')).toBe(expected.replace(/\s+/g, ''));
  });

  test('the browser view renders no card', async () => {
    const http = mockFetch().on('GET', STATUS, json({ largest_file: GB, uploads_paused: false }));
    const { container } = renderWithQuery(<StorageCard />);
    await waitFor(() => expect(http.callsTo('GET', STATUS)).toHaveLength(1));
    expect(container).toBeEmptyDOMElement();
  });

  test('"Change the limit" opens the one-field editor inside the card, and Close hides it', async () => {
    const SECTION = '/_/api/admin/config/section/advanced';
    mockFetch()
      .on('GET', STATUS, json(cases[0][1]))
      .on('GET', SECTION, json({}))
      .on('GET', '/_/api/admin/config', json({}));
    const user = userEvent.setup();
    renderWithQuery(<StorageCard />);
    expect(screen.queryByRole('radiogroup', { name: 'Largest file' })).not.toBeInTheDocument();
    await user.click(await screen.findByRole('button', { name: 'Change the limit' }));
    const card = screen.getByTestId('storage-card');
    expect(await within(card).findByRole('radiogroup', { name: 'Largest file' })).toBeInTheDocument();
    await user.click(within(card).getByRole('button', { name: 'Close' }));
    expect(screen.queryByRole('radiogroup', { name: 'Largest file' })).not.toBeInTheDocument();
  });
});

describe('the admin banner', () => {
  test.each([
    ['filling_up', 'The disk is filling up. Uploads will pause when it is full.'],
    ['large_paused', 'Large uploads are paused: the disk is full.'],
    ['backend_paused', 'Uploads are paused: the disk is full.'],
  ])('%s says its line and links to the fix', async (state, text) => {
    const body = cases.find(([n]) => n === state)![1];
    mockFetch().on('GET', STATUS, json(body));
    const onFix = vi.fn();
    renderWithQuery(<StorageBanner enabled onFix={onFix} />);
    expect(await screen.findByText(text)).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole('button', { name: /^Fix it →/ }));
    expect(onFix).toHaveBeenCalled();
  });

  test.each(['healthy', 'checking'])('is absent for %s', async (state) => {
    const http = mockFetch().on('GET', STATUS, json(cases.find(([n]) => n === state)![1]));
    const { container } = renderWithQuery(<StorageBanner enabled onFix={() => {}} />);
    await waitFor(() => expect(http.callsTo('GET', STATUS)).toHaveLength(1));
    expect(container).toBeEmptyDOMElement();
  });

  test('is absent for the browser view, even when paused', async () => {
    const http = mockFetch().on('GET', STATUS, json({ largest_file: GB, uploads_paused: true }));
    const { container } = renderWithQuery(<StorageBanner enabled onFix={() => {}} />);
    await waitFor(() => expect(http.callsTo('GET', STATUS)).toHaveLength(1));
    expect(container).toBeEmptyDOMElement();
  });

  test('asks nothing of a session that is not an admin', () => {
    const http = mockFetch();
    const { container } = renderWithQuery(<StorageBanner enabled={false} onFix={() => {}} />);
    expect(container).toBeEmptyDOMElement();
    expect(http.calls).toHaveLength(0);
  });
});

describe('plain upload refusals', () => {
  const PAUSED = "Uploads are paused. The server's disk is full. Try again later.";

  test('the S3 reason header of an AWS SDK error', () => {
    const sdk = Object.assign(new Error('Service Unavailable'), {
      name: 'ServiceUnavailable',
      $metadata: { httpStatusCode: 503 },
      $response: { headers: { 'x-deltaglider-reason': 'uploads-paused' } },
    });
    expect(plainError(normalizeS3Error(sdk, 'Upload object a'), 'x').text).toBe(PAUSED);
    expect(plainError(sdk, 'x').text).toBe(PAUSED);
  });

  test('the admin JSON reason', async () => {
    const res = json({ error: 'service_unavailable', message: 'x', reason: 'uploads-paused' }, 503);
    const err = await throwApiError(res, 'Copy').catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(plainError(err, 'x').text).toBe(PAUSED);
  });

  test('a plain 503 SlowDown has no paused sentence', () => {
    const sdk = Object.assign(new Error('slow'), { name: 'SlowDown', $metadata: { httpStatusCode: 503 } });
    expect(plainError(sdk, 'x').text).not.toBe(PAUSED);
  });

  test('EntityTooLarge names the limit the status poll read', () => {
    queryClient.setQueryData(qk.storageStatus(), { largest_file: 37 * GB, uploads_paused: false });
    const sdk = Object.assign(new Error('too big'), { name: 'EntityTooLarge', $metadata: { httpStatusCode: 413 } });
    expect(plainError(sdk, 'x').text).toBe('This file is too large. The limit is 37 GiB.');
  });
});

describe('the upload pre-check limit', () => {
  test('a non-admin session reads largest_file and ignores uploads_paused', async () => {
    mockFetch().on('GET', STATUS, json({ largest_file: 10 * GB, uploads_paused: true }));
    const { result } = renderHookWithQuery(() => useUploadLimits('releases', false));
    await waitFor(() => expect(result.current.maxObjectSize).toBe(10 * GB));
    const r = precheckUpload([{ name: 'a', size: 1 }], result.current);
    expect(r.accepted).toHaveLength(1);
    expect(r.refused).toHaveLength(0);
  });
});
