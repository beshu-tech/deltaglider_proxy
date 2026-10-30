/**
 * B56: an image the browser cannot decode said only "Failed to load image"
 * in a red alert. It now says what is wrong and offers the download.
 */
import { fireEvent, screen } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';

vi.mock('../s3client', () => ({
  getPresignedUrl: async () => 'http://proxy.test/media/brand/logo.png?sig=1',
  getDownloadUrl: async () => 'http://proxy.test/dl',
  downloadObject: async () => new Blob(['']),
}));

import FilePreview from '../components/FilePreview';

test('an unreadable image explains itself and offers Download', async () => {
  renderWithQuery(
    <FilePreview open object={{ key: 'brand/logo.png', size: 2048, lastModified: '', etag: '', headers: {} }} onClose={() => {}} />,
  );
  const img = await screen.findByAltText('logo.png');
  fireEvent.error(img);
  expect(await screen.findByText('This file is not a readable image.')).toBeInTheDocument();
  expect(screen.queryByAltText('logo.png')).toBeNull();
  expect(screen.getAllByRole('button', { name: /Download/ }).length).toBeGreaterThan(1);
});
