/**
 * Object table rows (UX harvest cycle 2):
 * C14 - a long name is cut in the middle: the distinguishing end stays;
 * C29 - a ticked row carries aria-selected (theme.css tints it).
 */
import { screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import ObjectTable from '../components/ObjectTable';
import type { S3Object } from '../types';
import { renderWithQuery } from '../test/render';

const LONG = 'very-long-object-name-segment-1-very-long-object-name-segment-2-app-linux-amd64.tar.gz';
const OBJECTS: S3Object[] = [
  { key: 'builds/app-v1.zip', size: 10, lastModified: '2026-01-01T00:00:00Z', storageType: 'delta' },
  { key: 'builds/app-v2.zip', size: 10, lastModified: '2026-01-01T00:00:00Z', storageType: 'delta' },
  { key: `builds/${LONG}`, size: 10, lastModified: '2026-01-01T00:00:00Z' },
] as S3Object[];

function mount() {
  renderWithQuery(
    <ObjectTable
      listing={{
        objects: OBJECTS,
        folders: [],
        prefix: 'builds/',
        isTruncated: false,
        refreshing: false,
        headCache: {
          'builds/app-v1.zip': { storageType: 'delta', isBaseline: true },
          'builds/app-v2.zip': { storageType: 'delta' },
        },
        virtualFolders: [],
      }}
      selection={{ inspected: null, selectedKeys: new Set(['builds/app-v2.zip']), onToggleKey: () => {}, onToggleAll: () => {} }}
      actions={{ onSelect: () => {}, onNavigate: () => {} }}
      folderSizes={{ sizes: {}, canScan: false, onCompute: () => {}, onCancel: () => {} }}
      isMobile={false}
    />,
  );
}

const row = (name: string) => screen.getByTestId(`object-row-${name}`).closest('.ant-table-row')!;

test('a long name keeps its end as a part that is never cut, and the full name as the title', () => {
  // TruncatedName splits a name only while it overflows: make every element
  // overflow (jsdom has no layout), then drop the overrides.
  Object.defineProperty(HTMLElement.prototype, 'scrollWidth', { configurable: true, get: () => 500 });
  Object.defineProperty(HTMLElement.prototype, 'clientWidth', { configurable: true, get: () => 100 });
  try {
    mount();
    const cell = screen.getByTestId(`object-row-${LONG}`);
    expect(cell).toHaveAttribute('title', LONG);
    expect(screen.getByText('-linux-amd64.tar.gz', { exact: false })).toHaveStyle({ flexShrink: '0' });
  } finally {
    delete (HTMLElement.prototype as { scrollWidth?: number }).scrollWidth;
    delete (HTMLElement.prototype as { clientWidth?: number }).clientWidth;
  }
});

test('a ticked row is aria-selected, an unticked one is not', () => {
  mount();
  expect(row('app-v2.zip')).toHaveAttribute('aria-selected', 'true');
  expect(row('app-v1.zip')).toHaveAttribute('aria-selected', 'false');
});
