/**
 * Object browser keyboard + row handling: ObjectTable wired to
 * useBrowserKeyboardNav the way App wires them. Arrow keys move the cursor
 * row in displayed order, Enter opens it (folder → navigate, file →
 * inspector), Escape goes up; typing in a field or an open overlay owns the
 * keys; the checkbox cell selects without opening the inspector.
 */
import { fireEvent, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Modal } from 'antd';
import { describe, expect, test, vi, type Mock } from 'vitest';
import ObjectTable from '../components/ObjectTable';
import type { S3Object } from '../types';
import { useBrowserKeyboardNav } from '../useBrowserKeyboardNav';
import { renderWithQuery } from '../test/render';

const OBJECTS: S3Object[] = [
  { key: 'builds/app-1.0.zip', size: 10, lastModified: '2026-01-01T00:00:00Z' },
  { key: 'builds/app-1.1.zip', size: 20, lastModified: '2026-01-02T00:00:00Z' },
] as S3Object[];
const FOLDERS = ['builds/nightly/'];

type KeyFn = (key: string) => void;
interface Spies {
  navigate: Mock<KeyFn>;
  openInspector: Mock<KeyFn>;
  onSelect: Mock<(obj: S3Object) => void>;
  onToggleKey: Mock<KeyFn>;
}

function Harness({ spies, extra }: { spies: Spies; extra?: React.ReactNode }) {
  const nav = useBrowserKeyboardNav({
    folders: FOLDERS,
    objects: OBJECTS,
    prefix: 'builds/',
    navigate: spies.navigate,
    openInspector: spies.openInspector,
    enabled: true,
  });
  return (
    <>
      {extra}
      <ObjectTable
        listing={{
          objects: OBJECTS,
          folders: FOLDERS,
          prefix: 'builds/',
          isTruncated: false,
          refreshing: false,
          headCache: {},
          virtualFolders: [],
        }}
        selection={{ inspected: null, selectedKeys: new Set(), onToggleKey: spies.onToggleKey, onToggleAll: () => {} }}
        actions={{ onSelect: spies.onSelect, onNavigate: spies.navigate }}
        folderSizes={{ sizes: {}, canScan: false, onCompute: () => {}, onCancel: () => {} }}
        cursor={{ key: nav.cursorKey, onChange: nav.setCursorKey, onRowOrderChange: nav.setRowOrder }}
        isMobile={false}
      />
    </>
  );
}

function mount(extra?: React.ReactNode) {
  const spies: Spies = {
    navigate: vi.fn<KeyFn>(),
    openInspector: vi.fn<KeyFn>(),
    onSelect: vi.fn<(obj: S3Object) => void>(),
    onToggleKey: vi.fn<KeyFn>(),
  };
  renderWithQuery(<Harness spies={spies} extra={extra} />);
  return spies;
}

/** The data row for a key (AntD sets data-row-key on each row). */
function row(key: string): HTMLElement {
  const el = document.querySelector<HTMLElement>(`[data-row-key="${key}"]`);
  if (!el) throw new Error(`no row ${key}`);
  return el;
}
const cursorRow = () => document.querySelector('.dg-row-cursor')?.getAttribute('data-row-key') ?? null;

describe('keyboard', () => {
  test('arrows walk folders then files; Home/End jump; Enter opens', async () => {
    const spies = mount();
    await screen.findByText('app-1.0.zip');
    fireEvent.keyDown(document, { key: 'ArrowDown' });
    await waitFor(() => expect(cursorRow()).toBe('folder:builds/nightly/'));
    fireEvent.keyDown(document, { key: 'ArrowDown' });
    await waitFor(() => expect(cursorRow()).toBe('builds/app-1.0.zip'));
    fireEvent.keyDown(document, { key: 'End' });
    await waitFor(() => expect(cursorRow()).toBe('builds/app-1.1.zip'));
    fireEvent.keyDown(document, { key: 'Enter' });
    expect(spies.openInspector).toHaveBeenCalledWith('builds/app-1.1.zip');
    fireEvent.keyDown(document, { key: 'Home' });
    await waitFor(() => expect(cursorRow()).toBe('folder:builds/nightly/'));
    fireEvent.keyDown(document, { key: 'Enter' });
    expect(spies.navigate).toHaveBeenCalledWith('builds/nightly/');
  });

  test('Escape goes up one folder', async () => {
    const spies = mount();
    await screen.findByText('app-1.0.zip');
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(spies.navigate).toHaveBeenCalledWith('');
  });

  test('keys typed into a field are not hijacked', async () => {
    const user = userEvent.setup();
    const spies = mount(<input aria-label="Search" />);
    await screen.findByText('app-1.0.zip');
    await user.click(screen.getByLabelText('Search'));
    await user.keyboard('{ArrowDown}{Enter}{Escape}{Backspace}');
    expect(cursorRow()).toBeNull();
    expect(spies.navigate).not.toHaveBeenCalled();
    expect(spies.openInspector).not.toHaveBeenCalled();
  });

  test('an open modal owns the keys', async () => {
    const spies = mount(<Modal open title="Busy">modal body</Modal>);
    await screen.findByText('modal body');
    fireEvent.keyDown(document, { key: 'ArrowDown' });
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(cursorRow()).toBeNull();
    expect(spies.navigate).not.toHaveBeenCalled();
  });
});

describe('mouse', () => {
  test('a row click selects the file and moves the cursor there', async () => {
    const user = userEvent.setup();
    const spies = mount();
    await user.click(await screen.findByText('app-1.1.zip'));
    expect(spies.onSelect).toHaveBeenCalledWith(expect.objectContaining({ key: 'builds/app-1.1.zip' }));
    await waitFor(() => expect(cursorRow()).toBe('builds/app-1.1.zip'));
  });

  test('the checkbox cell toggles selection without opening the inspector', async () => {
    const user = userEvent.setup();
    const spies = mount();
    await screen.findByText('app-1.0.zip');
    await user.click(screen.getByRole('checkbox', { name: 'Select app-1.0.zip' }));
    expect(spies.onToggleKey.mock.calls).toEqual([['builds/app-1.0.zip']]);
    // The cell around the box is a hit target too (the virtual table renders
    // rows and cells as divs).
    const cell = row('builds/app-1.1.zip').querySelector<HTMLElement>('.ant-table-cell');
    expect(cell).not.toBeNull();
    await user.click(cell!);
    expect(spies.onToggleKey.mock.calls).toEqual([['builds/app-1.0.zip'], ['builds/app-1.1.zip']]);
    expect(spies.onSelect).not.toHaveBeenCalled();
  });

  test('a folder name opens the folder', async () => {
    const user = userEvent.setup();
    const spies = mount();
    await user.click(await screen.findByRole('button', { name: /nightly/ }));
    expect(spies.navigate).toHaveBeenCalledWith('builds/nightly/');
    expect(spies.onSelect).not.toHaveBeenCalled();
  });
});

describe('assistive tech', () => {
  test('the list is a grid of rows and cells, with one row in the tab order', async () => {
    mount();
    await screen.findByText('app-1.0.zip');
    const grid = screen.getByRole('grid', { name: 'Objects in builds/' });
    expect(grid).toHaveAttribute('aria-rowcount', '4');
    expect(grid.querySelector('table')).toHaveAttribute('role', 'presentation');
    expect(screen.getAllByRole('columnheader').map((h) => h.textContent)).toContain('Name');
    const rows = document.querySelectorAll('.ant-table-row');
    expect([...rows].map((r) => r.getAttribute('role'))).toEqual(['row', 'row', 'row']);
    expect([...rows].map((r) => r.getAttribute('aria-rowindex'))).toEqual(['2', '3', '4']);
    expect([...rows].map((r) => r.getAttribute('tabindex'))).toEqual(['0', '-1', '-1']);
    expect(row('builds/app-1.0.zip').querySelectorAll('[role="gridcell"]').length).toBeGreaterThan(2);
  });

  test('a focused row moves the cursor, and the focus follows the arrows', async () => {
    mount();
    await screen.findByText('app-1.0.zip');
    row('builds/app-1.0.zip').focus();
    await waitFor(() => expect(cursorRow()).toBe('builds/app-1.0.zip'));
    fireEvent.keyDown(document.activeElement!, { key: 'ArrowDown' });
    await waitFor(() => expect(cursorRow()).toBe('builds/app-1.1.zip'));
    await waitFor(() => expect(document.activeElement).toBe(row('builds/app-1.1.zip')));
    expect(row('builds/app-1.1.zip')).toHaveAttribute('tabindex', '0');
  });
});

test('Enter on a focused button presses it and does not open the cursor row', async () => {
  const clicked = vi.fn();
  const spies = mount(<button onClick={clicked}>Upload Files</button>);
  await screen.findByText('app-1.0.zip');
  fireEvent.keyDown(document, { key: 'End' });
  await waitFor(() => expect(cursorRow()).toBe('builds/app-1.1.zip'));
  const button = screen.getByRole('button', { name: 'Upload Files' });
  button.focus();
  fireEvent.keyDown(button, { key: 'Enter' });
  expect(spies.openInspector).not.toHaveBeenCalled();
});
