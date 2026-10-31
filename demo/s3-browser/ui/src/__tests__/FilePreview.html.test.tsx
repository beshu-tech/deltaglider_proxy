import { fireEvent, screen } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import { renderWithQuery } from '../test/render';

const PAGE = '<!doctype html><html><body><h1>Release notes</h1><script>parent.document.title="x"</script></body></html>';

vi.mock('../s3client', () => ({
  getPresignedUrl: async () => 'http://proxy.test/x',
  getDownloadUrl: async () => 'http://proxy.test/dl',
  downloadObject: async () => new Blob([PAGE]),
}));

import FilePreview from '../components/FilePreview';

const object = (size = PAGE.length) => ({ key: 'site/notes.html', size, lastModified: '', etag: '', headers: {} });

test('an HTML object renders in an isolated iframe, scripts off', async () => {
  renderWithQuery(<FilePreview open object={object()} onClose={() => {}} />);
  const frame = await screen.findByTitle('Preview of notes.html');
  expect(frame.getAttribute('sandbox')).toBe('');
  const doc = frame.getAttribute('srcdoc') ?? '';
  expect(doc).toContain("script-src 'none'");
  expect(doc).toContain('<h1>Release notes</h1>');
});

test('Run scripts allows scripts but never the same origin', async () => {
  renderWithQuery(<FilePreview open object={object()} onClose={() => {}} />);
  await screen.findByTitle('Preview of notes.html');
  fireEvent.click(screen.getByRole('switch', { name: 'Run scripts' }));
  const frame = await screen.findByTitle('Preview of notes.html');
  expect(frame.getAttribute('sandbox')).toBe('allow-scripts');
  expect(frame.getAttribute('srcdoc')).toContain("connect-src 'none'");
});

test('Source shows the markup as text', async () => {
  renderWithQuery(<FilePreview open object={object()} onClose={() => {}} />);
  await screen.findByTitle('Preview of notes.html');
  fireEvent.click(screen.getByText('Source'));
  expect((await screen.findByLabelText('Source of notes.html')).textContent).toBe(PAGE);
  expect(screen.queryByTitle('Preview of notes.html')).toBeNull();
});

test('a page above the limit offers the download instead', async () => {
  renderWithQuery(<FilePreview open object={object(6 * 1024 * 1024)} onClose={() => {}} />);
  expect(await screen.findByText(/File too large for preview/)).toBeInTheDocument();
  expect(screen.queryByTitle('Preview of notes.html')).toBeNull();
});
