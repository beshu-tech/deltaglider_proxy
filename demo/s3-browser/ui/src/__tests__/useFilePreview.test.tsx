// useFilePreview: the preview overlay follows ?preview=1 in the URL.
import { act } from '@testing-library/react';
import { expect, test, vi } from 'vitest';
import { renderHookWithQuery } from '../test/render';
import { useFilePreview } from '../browser/useFilePreview';
import { parseBrowserLocation, type BrowserLocation } from '../urlState';
import type { S3Object } from '../types';

const NOTES = { key: 'builds/notes.txt', size: 5, lastModified: '' } as S3Object;
const at = (over: Partial<BrowserLocation> = {}): BrowserLocation => ({
  bucket: 'releases',
  prefix: 'builds/',
  q: '',
  object: '',
  preview: '',
  ...over,
});

/** The hook under a fake router: navigate re-parses the URL into the location. */
function mount(initial: BrowserLocation) {
  let loc = initial;
  const navigate = vi.fn((url: string) => {
    const [path, query = ''] = url.split('?');
    loc = parseBrowserLocation(path, query ? `?${query}` : '');
  });
  const view = renderHookWithQuery(() => useFilePreview(loc, navigate, [NOTES]));
  return {
    ...view,
    navigate,
    setLoc(next: BrowserLocation) {
      loc = next;
      view.rerender();
    },
  };
}

test('open shows the object and pushes ?object=…&preview=1', () => {
  const { result, navigate, rerender } = mount(at());
  act(() => result.current.openPreview(NOTES));
  rerender();
  expect(result.current.previewObject).toBe(NOTES);
  expect(navigate).toHaveBeenCalledWith('/_/browse/releases/builds/?object=builds%2Fnotes.txt&preview=1');
});

test('a direct load with ?preview=1 opens the listed object', () => {
  const { result } = mount(at({ object: NOTES.key, preview: '1' }));
  expect(result.current.previewObject).toBe(NOTES);
});

test('Back (the flag leaves the URL) closes it', () => {
  const { result, setLoc } = mount(at({ object: NOTES.key, preview: '1' }));
  setLoc(at({ object: NOTES.key }));
  expect(result.current.previewObject).toBeNull();
});

test('close of a direct load replaces the URL and keeps ?object= for the inspector', () => {
  const { result, navigate, rerender } = mount(at({ object: NOTES.key, preview: '1' }));
  act(() => result.current.closePreview());
  rerender();
  expect(navigate).toHaveBeenCalledWith('/_/browse/releases/builds/?object=builds%2Fnotes.txt', { replace: true });
  expect(result.current.previewObject).toBeNull();
});
