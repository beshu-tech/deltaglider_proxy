// useBrowserLocation: folder/bucket navigation, the inspector deep link and
// the debounced ?q= write, all through the router's navigate.
import { act } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { renderHookWithQuery } from '../test/render';

const s3 = vi.hoisted(() => ({ bucket: 'releases' }));
vi.mock('../s3client', () => ({
  getBucket: () => s3.bucket,
  setBucket: (b: string) => {
    s3.bucket = b;
  },
}));

import { useBrowserLocation } from '../browser/useBrowserLocation';

type Loc = { bucket: string; prefix: string; q: string; object: string };

function mount(initial: Loc) {
  let loc = initial;
  const navigateUrl = vi.fn();
  const view = renderHookWithQuery(() => useBrowserLocation({ ...loc, navigateUrl }));
  return {
    ...view,
    navigateUrl,
    setLoc(next: Partial<Loc>) {
      loc = { ...loc, ...next };
      view.rerender();
    },
  };
}

beforeEach(() => {
  s3.bucket = 'releases';
});
afterEach(() => vi.useRealTimers());

describe('navigation', () => {
  test('a folder pushes its URL and drops the search', () => {
    const { result, navigateUrl } = mount({ bucket: 'releases', prefix: '', q: 'app', object: '' });
    act(() => result.current.navigate('builds/'));
    expect(navigateUrl).toHaveBeenCalledWith('/_/browse/releases/builds/');
  });

  test('at bare /_/browse the folder uses the s3client bucket (v1.3.1 fix)', () => {
    const { result, navigateUrl } = mount({ bucket: '', prefix: '', q: '', object: '' });
    act(() => result.current.navigate('builds/'));
    expect(navigateUrl).toHaveBeenCalledWith('/_/browse/releases/builds/');
  });

  test('a bucket switch syncs s3client and opens its root', () => {
    const { result, navigateUrl } = mount({ bucket: 'releases', prefix: 'builds/', q: '', object: '' });
    act(() => result.current.changeBucket('downloads'));
    expect(s3.bucket).toBe('downloads');
    expect(navigateUrl).toHaveBeenCalledWith('/_/browse/downloads/');
  });

  test('the URL bucket is synced into s3client', () => {
    s3.bucket = '';
    mount({ bucket: 'db-archive', prefix: '', q: '', object: '' });
    expect(s3.bucket).toBe('db-archive');
  });
});

describe('inspector', () => {
  test('open pushes ?object= and keeps the folder and search', () => {
    const { result, navigateUrl } = mount({ bucket: 'releases', prefix: 'builds/', q: 'app', object: '' });
    act(() => result.current.openInspector('builds/app.zip'));
    expect(navigateUrl).toHaveBeenCalledWith('/_/browse/releases/builds/?q=app&object=builds%2Fapp.zip');
  });

  test('close of a direct-loaded deep link replaces the URL', () => {
    const { result, navigateUrl } = mount({ bucket: 'releases', prefix: 'builds/', q: '', object: 'builds/app.zip' });
    act(() => result.current.closeInspector());
    expect(navigateUrl).toHaveBeenCalledWith('/_/browse/releases/builds/', { replace: true });
  });

  test('close with nothing open does nothing', () => {
    const { result, navigateUrl } = mount({ bucket: 'releases', prefix: '', q: '', object: '' });
    act(() => result.current.closeInspector());
    expect(navigateUrl).not.toHaveBeenCalled();
  });
});

describe('search', () => {
  test('the box updates at once; the URL follows 200 ms later as one replace', () => {
    vi.useFakeTimers();
    const { result, navigateUrl } = mount({ bucket: 'releases', prefix: 'builds/', q: '', object: '' });
    act(() => result.current.setSearchQuery('a'));
    act(() => result.current.setSearchQuery('ap'));
    expect(result.current.searchQuery).toBe('ap');
    expect(navigateUrl).not.toHaveBeenCalled();
    act(() => {
      vi.advanceTimersByTime(200);
    });
    expect(navigateUrl).toHaveBeenCalledTimes(1);
    expect(navigateUrl).toHaveBeenCalledWith('/_/browse/releases/builds/?q=ap', { replace: true });
  });

  test('the debounced write reads the latest folder, not the one at the keystroke', () => {
    vi.useFakeTimers();
    const { result, navigateUrl, setLoc } = mount({ bucket: 'releases', prefix: 'builds/', q: '', object: '' });
    act(() => result.current.setSearchQuery('x'));
    setLoc({ prefix: 'nightly/' });
    act(() => {
      vi.advanceTimersByTime(200);
    });
    expect(navigateUrl).toHaveBeenCalledWith('/_/browse/releases/nightly/?q=x', { replace: true });
  });

  test('an external ?q= change resets the box when the user is not typing', () => {
    const { result, setLoc } = mount({ bucket: 'releases', prefix: '', q: 'old', object: '' });
    expect(result.current.searchQuery).toBe('old');
    setLoc({ q: 'new' });
    expect(result.current.searchQuery).toBe('new');
  });
});
