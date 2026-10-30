/**
 * ObjectTable's prop groups reach the rows: the inspected row is
 * highlighted, the Size column follows the folder-size group (scan only
 * with an admin session, never on a virtual folder), a double-click
 * previews a previewable file, and the status bar names stale rows.
 */
import { fireEvent, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import ObjectTable from '../components/ObjectTable';
import type { S3Object } from '../types';
import { renderWithQuery } from '../test/render';

const OBJECTS = [
  { key: 'builds/notes.txt', size: 2048, lastModified: '2026-01-01T00:00:00Z' },
  { key: 'builds/app.zip', size: 10, lastModified: '2026-01-02T00:00:00Z' },
] as S3Object[];
const FOLDERS = ['builds/nightly/', 'builds/ci/'];

function mount(over: { canScan?: boolean; inspected?: S3Object | null; restListedAt?: number | null } = {}) {
  const spies = {
    onCompute: vi.fn(),
    onPreview: vi.fn(),
    onToggleAll: vi.fn(),
  };
  renderWithQuery(
    <ObjectTable
      listing={{
        objects: OBJECTS,
        folders: FOLDERS,
        prefix: 'builds/',
        isTruncated: false,
        restListedAt: over.restListedAt ?? null,
        refreshing: false,
        headCache: {},
        virtualFolders: ['builds/ci/'],
      }}
      selection={{
        inspected: over.inspected ?? null,
        selectedKeys: new Set(),
        onToggleKey: () => {},
        onToggleAll: spies.onToggleAll,
      }}
      actions={{ onSelect: () => {}, onNavigate: () => {}, onPreview: spies.onPreview }}
      folderSizes={{ sizes: {}, canScan: over.canScan ?? false, onCompute: spies.onCompute, onCancel: () => {} }}
      isMobile={false}
    />,
  );
  return spies;
}

function row(key: string): HTMLElement {
  const el = document.querySelector<HTMLElement>(`[data-row-key="${key}"]`);
  if (!el) throw new Error(`no row ${key}`);
  return el;
}

test('the inspected object is the highlighted row', async () => {
  mount({ inspected: OBJECTS[1] });
  await screen.findByText('notes.txt');
  expect(row('builds/app.zip').className).toContain('ant-table-row-selected');
  expect(row('builds/notes.txt').className).not.toContain('ant-table-row-selected');
});

test('without an admin session a folder size is a dash', async () => {
  mount({ canScan: false });
  await screen.findByText('notes.txt');
  expect(within(row('folder:builds/nightly/')).getByTitle(/sign in as an administrator/)).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: /^Size$/ })).toBeNull();
});

test('with an admin session the Size button scans the folder; a virtual folder never', async () => {
  const user = userEvent.setup();
  const spies = mount({ canScan: true });
  await screen.findByText('notes.txt');
  await user.click(within(row('folder:builds/nightly/')).getByRole('button', { name: /Size/ }));
  expect(spies.onCompute).toHaveBeenCalledWith('builds/nightly/');
  expect(within(row('folder:builds/ci/')).getByText('Virtual')).toBeInTheDocument();
});

test('a double-click previews a text file, not an archive', async () => {
  const spies = mount();
  fireEvent.doubleClick(await screen.findByText('notes.txt'));
  expect(spies.onPreview).toHaveBeenCalledWith(expect.objectContaining({ key: 'builds/notes.txt' }));
  fireEvent.doubleClick(screen.getByText('app.zip'));
  expect(spies.onPreview).toHaveBeenCalledTimes(1);
});

test('the select-all box calls the selection group', async () => {
  const user = userEvent.setup();
  const spies = mount();
  await screen.findByText('notes.txt');
  await user.click(screen.getByLabelText('Select all'));
  expect(spies.onToggleAll).toHaveBeenCalledTimes(1);
});

test('the status bar says when the rows after the first page were listed', async () => {
  mount({ restListedAt: new Date('2026-01-01T10:30:00').getTime() });
  await screen.findByText('notes.txt');
  expect(screen.getByRole('status').textContent).toMatch(/entries after the first 1,000 as of/);
});
