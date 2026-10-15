/**
 * Sidebar rows: an encrypted bucket carries a padlock (hover text says how),
 * a plaintext one none; and a short backend name gets a frame no wider than
 * the name (a 40 px minimum made "b2" sit in a wide empty box).
 */
import { screen } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import Sidebar from '../components/Sidebar';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', async (importOriginal) => {
  const real = await importOriginal<typeof import('../s3client')>();
  return {
    ...real,
    listBuckets: async () => [
      { name: 'releases', creationDate: '', backend: { backendName: 'hetzner-fsn1' } },
      {
        name: 'db-archive', creationDate: '',
        backend: { backendName: 'b2', encryptedAtRest: 'Encrypted at rest: proxy AES-256-GCM, key k1 (set on this bucket)' },
      },
    ],
  };
});

test('the padlock marks only the encrypted bucket; a short chip is tight', async () => {
  renderWithQuery(
    <Sidebar
      onUploadClick={() => {}} onBucketChange={() => {}} canCreateBucket canDeleteBucket={() => false}
      canUpload canAdmin open onClose={() => {}} isMobile={false}
    />,
  );
  const lock = await screen.findByRole('img', { name: 'Encrypted' });
  expect(lock).toHaveAttribute('title', 'Encrypted at rest: proxy AES-256-GCM, key k1 (set on this bucket)');
  expect(lock.closest('li')).toHaveTextContent('db-archive');
  expect(screen.getAllByRole('img', { name: 'Encrypted' })).toHaveLength(1);
  expect(screen.getByLabelText('Backend b2').style.minWidth).toBe('min(40px, 2ch + 14px)');
});
