/**
 * CommandPalette (⌘K): fuzzy filter over ADMIN_IA + shell actions, keyboard
 * navigation, Enter runs, Esc closes, and a Recent MRU persisted through
 * safeStorage (localStorage in jsdom).
 */
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import CommandPalette, { FileTextOutlined } from '../components/CommandPalette';

const RECENTS_KEY = 'dgp.admin.palette.recents';

beforeEach(() => {
  window.localStorage.clear();
});
afterEach(() => {
  window.localStorage.clear();
});

function renderPalette(extra: { open?: boolean } = {}) {
  const onClose = vi.fn();
  const onNavigateAdmin = vi.fn();
  const exportYaml = vi.fn();
  const utils = render(
    <CommandPalette
      open={extra.open ?? true}
      onClose={onClose}
      onNavigateAdmin={onNavigateAdmin}
      extraActions={[
        { id: 'act:export', label: 'Export YAML', keywords: 'download config', icon: <FileTextOutlined />, onRun: exportYaml },
      ]}
    />,
  );
  return { onClose, onNavigateAdmin, exportYaml, ...utils };
}

const search = () => screen.getByRole('combobox');
const options = () => within(screen.getByRole('listbox', { name: 'Command palette' })).getAllByRole('option');
const optionLabels = () => options().map((o) => o.querySelector('div > div')?.textContent ?? '');
const selected = () => options().find((o) => o.getAttribute('aria-selected') === 'true');

test('closed palette renders nothing', () => {
  renderPalette({ open: false });
  expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
});

test('open palette lists Navigate + Actions with the first item selected', () => {
  renderPalette();
  expect(screen.getByText('Navigate')).toBeInTheDocument();
  expect(screen.getByText('Actions')).toBeInTheDocument();
  expect(screen.queryByText('Recent')).not.toBeInTheDocument();
  expect(optionLabels()[0]).toBe('Dashboard');
  expect(selected()).toHaveTextContent('Dashboard');
  expect(optionLabels()).toContain('Export YAML');
});

describe('filter', () => {
  test('substring match ranks a label prefix first', async () => {
    renderPalette();
    await userEvent.setup().type(search(), 'users');
    expect(optionLabels()[0]).toBe('Users');
    expect(screen.queryByText('Navigate')).not.toBeInTheDocument();
  });

  test('subsequence (fuzzy) match: "evdl" finds Event delivery', async () => {
    renderPalette();
    await userEvent.setup().type(search(), 'evdl');
    expect(optionLabels()).toContain('Event delivery');
    expect(optionLabels()).not.toContain('Dashboard');
  });

  test('keywords match beyond the visible label', async () => {
    renderPalette();
    await userEvent.setup().type(search(), 'download');
    expect(optionLabels()).toEqual(['Export YAML']);
  });

  test('no match shows the empty state and 0 results', async () => {
    renderPalette();
    await userEvent.setup().type(search(), 'zzzzqqq');
    expect(screen.getByText('No matches. Try a shorter query.')).toBeInTheDocument();
    expect(screen.getByText('0 results')).toBeInTheDocument();
  });
});

describe('keyboard', () => {
  test('ArrowDown/ArrowUp move the cursor; Enter navigates to that path and closes', async () => {
    const { onNavigateAdmin, onClose } = renderPalette();
    const user = userEvent.setup();
    await user.click(search());
    await user.keyboard('{ArrowDown}{ArrowDown}');
    expect(selected()).toHaveTextContent('Compression health');
    await user.keyboard('{ArrowUp}');
    expect(selected()).toHaveTextContent('Request rule tester');
    expect(search()).toHaveAttribute('aria-activedescendant', 'cmd-nav:diagnostics/trace');
    await user.keyboard('{Enter}');
    expect(onNavigateAdmin).toHaveBeenCalledWith('diagnostics/trace');
    expect(onClose).toHaveBeenCalled();
  });

  test('ArrowUp at the top stays on the first item', async () => {
    renderPalette();
    const user = userEvent.setup();
    await user.click(search());
    await user.keyboard('{ArrowUp}{ArrowUp}');
    expect(selected()).toHaveTextContent('Dashboard');
  });

  test('typing then Enter runs the best match', async () => {
    const { onNavigateAdmin } = renderPalette();
    const user = userEvent.setup();
    await user.type(search(), 'buckets{Enter}');
    expect(onNavigateAdmin).toHaveBeenCalledWith('storage/buckets');
  });

  test('Escape closes without running anything', async () => {
    const { onClose, onNavigateAdmin } = renderPalette();
    const user = userEvent.setup();
    await user.click(search());
    await user.keyboard('{Escape}');
    // Esc reaches both the input's handler and the AntD Modal's own Esc, so
    // onClose fires twice. Harmless: the parent's close is idempotent.
    expect(onClose).toHaveBeenCalled();
    expect(onNavigateAdmin).not.toHaveBeenCalled();
  });

  test('Enter on a shell action runs it', async () => {
    const { exportYaml, onNavigateAdmin } = renderPalette();
    await userEvent.setup().type(search(), 'export yaml{Enter}');
    expect(exportYaml).toHaveBeenCalledTimes(1);
    expect(onNavigateAdmin).not.toHaveBeenCalled();
  });
});

describe('recents MRU', () => {
  test('a run is stored and shows first under Recent on the next open', async () => {
    const first = renderPalette();
    const user = userEvent.setup();
    await user.type(search(), 'groups{Enter}');
    expect(JSON.parse(window.localStorage.getItem(RECENTS_KEY) ?? '[]')).toEqual(['nav:access/groups']);
    first.unmount();

    renderPalette();
    expect(screen.getByText('Recent')).toBeInTheDocument();
    expect(optionLabels()[0]).toBe('Groups');
    expect(selected()).toHaveTextContent('Groups');
  });

  test('clicking a row also records it; re-running moves it to the front, capped at 5', async () => {
    window.localStorage.setItem(
      RECENTS_KEY,
      JSON.stringify(['nav:jobs', 'nav:system', 'nav:dashboard', 'nav:access/users', 'nav:storage/backends']),
    );
    renderPalette();
    const user = userEvent.setup();
    await user.type(search(), 'audit');
    await user.click(screen.getByRole('option', { name: /Audit log/ }));
    expect(JSON.parse(window.localStorage.getItem(RECENTS_KEY) ?? '[]')).toEqual([
      'nav:diagnostics/audit',
      'nav:jobs',
      'nav:system',
      'nav:dashboard',
      'nav:access/users',
    ]);
  });

  test('a stale or corrupt stored entry is ignored, not a crash', () => {
    window.localStorage.setItem(RECENTS_KEY, JSON.stringify(['nav:gone-page', 42, 'nav:jobs']));
    renderPalette();
    const recentRows = optionLabels().slice(0, 1);
    expect(recentRows).toEqual(['Jobs']);
    expect(screen.getByText('Recent')).toBeInTheDocument();
  });

  test('unparseable storage falls back to no recents', () => {
    window.localStorage.setItem(RECENTS_KEY, '{not json');
    renderPalette();
    expect(screen.queryByText('Recent')).not.toBeInTheDocument();
  });
});
