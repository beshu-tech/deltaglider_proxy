/**
 * Sortable table headers: AntD makes them focusable and sorts on Enter only,
 * and the focus ring was not visible. Space sorts too now (as on a button),
 * and the header draws an inset focus ring in both themes.
 */
import { fireEvent, render, screen } from '@testing-library/react';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { Table } from 'antd';
import { expect, test, vi } from 'vitest';
import { withKeyboardSort } from '../tableSort';

interface Row { key: string; name: string }

function renderTable(onChange: (...args: unknown[]) => void) {
  const columns = withKeyboardSort<Row>([{ key: 'name', dataIndex: 'name', title: 'Name', sorter: true }]);
  render(
    <Table<Row>
      columns={columns}
      dataSource={[{ key: 'a', name: 'b.zip' }, { key: 'b', name: 'a.zip' }]}
      pagination={false}
      onChange={onChange}
    />,
  );
  return screen.getByRole('columnheader', { name: 'Name' });
}

test('Space on a focused sortable header sorts, and does not scroll the page', () => {
  const onChange = vi.fn();
  const header = renderTable(onChange);
  expect(header).toHaveAttribute('tabindex', '0');
  header.focus();
  const down = fireEvent.keyDown(header, { key: ' ', code: 'Space', keyCode: 32 });
  expect(down).toBe(false); // default (page scroll) prevented
  expect(onChange).toHaveBeenCalledTimes(1);
  expect(onChange.mock.calls[0][2]).toMatchObject({ columnKey: 'name', order: 'ascend' });
});

test('Enter still sorts once (AntD handles it; the helper adds nothing)', () => {
  const onChange = vi.fn();
  const header = renderTable(onChange);
  fireEvent.keyDown(header, { key: 'Enter', code: 'Enter', keyCode: 13 });
  expect(onChange).toHaveBeenCalledTimes(1);
});

test('theme.css draws an inset focus ring on sortable headers', () => {
  const css = readFileSync(join(__dirname, '../theme.css'), 'utf8');
  expect(css).toMatch(/th\.ant-table-column-has-sorters:focus-visible\s*\{[^}]*outline:\s*2px solid var\(--focus-ring\)[^}]*outline-offset:\s*-2px/);
});

test('every component with a sortable column wraps its columns in withKeyboardSort', () => {
  const dir = join(__dirname, '../components');
  const files = readdirSync(dir, { recursive: true, encoding: 'utf8' }).filter((f) => f.endsWith('.tsx'));
  const sortable = files.filter((f) => /\bsorter:\s*(true|\()/.test(readFileSync(join(dir, f), 'utf8')));
  expect(sortable.length).toBeGreaterThan(0);
  for (const file of sortable) {
    expect(readFileSync(join(dir, file), 'utf8'), file).toMatch(/withKeyboardSort\(/);
  }
});
