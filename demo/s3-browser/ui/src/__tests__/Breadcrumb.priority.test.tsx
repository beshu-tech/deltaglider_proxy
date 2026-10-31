/**
 * UX harvest D6: on a tablet the header cut the breadcrumb from the right,
 * so the CURRENT folder disappeared while the middle folders stayed. When
 * the path does not fit, the middle folders collapse into one "…" link to
 * the parent folder, and the current folder stays.
 */
import { screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import Breadcrumb from '../components/Breadcrumb';
import { crumbSegments } from '../browser/crumbs';
import { renderWithQuery } from '../test/render';

afterEach(() => vi.restoreAllMocks());

test('the full path while it fits', () => {
  renderWithQuery(<Breadcrumb bucket="releases" prefix="qa-deep/level2/level3/level4/" onNavigate={() => {}} />);
  expect(screen.getByRole('link', { name: 'level2' })).toBeInTheDocument();
  expect(screen.getByText('level4')).toBeInTheDocument();
});

test('a path that overflows the header keeps the current folder and collapses the middle', () => {
  // jsdom has no layout: the list is 300 px wide and its content 900 px.
  vi.spyOn(HTMLElement.prototype, 'clientWidth', 'get').mockReturnValue(300);
  vi.spyOn(HTMLElement.prototype, 'scrollWidth', 'get').mockImplementation(function (this: HTMLElement) {
    return this.tagName === 'OL' ? 900 : 0;
  });
  renderWithQuery(<Breadcrumb bucket="releases" prefix="qa-deep/level2/level3/level4/" onNavigate={() => {}} />);
  expect(screen.queryByRole('link', { name: 'level2' })).toBeNull();
  const parent = screen.getByRole('link', { name: 'Parent folder' });
  expect(parent).toHaveAttribute('href', expect.stringContaining('qa-deep/level2/level3/'));
  expect(screen.getByText('level4').closest('[aria-current="location"]')).not.toBeNull();
  expect(screen.getByRole('link', { name: 'releases' })).toBeInTheDocument();
});

test('crumbSegments: the middle becomes one link to the parent folder', () => {
  const all = [
    { label: 'a', prefix: 'a/' },
    { label: 'b', prefix: 'a/b/' },
    { label: 'c', prefix: 'a/b/c/' },
  ];
  expect(crumbSegments(all, false)).toEqual(all);
  expect(crumbSegments(all, true)).toEqual([{ label: '…', prefix: 'a/b/', collapsed: true }, { label: 'c', prefix: 'a/b/c/' }]);
});
