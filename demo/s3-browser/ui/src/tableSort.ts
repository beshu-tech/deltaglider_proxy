/**
 * Keyboard support for AntD sortable table headers. AntD gives a sortable
 * `th` tabIndex 0 and sorts on Enter only, and it calls a column's own
 * `onKeyDown` only for Enter. Space must sort too (the header acts as a
 * button), so the helper uses a handler AntD leaves alone, the key-down
 * capture phase: it stops the page scroll and clicks the header, which
 * runs AntD's own sort toggle.
 *
 * Wrap every `columns` array that has a `sorter` (a source test in
 * src/__tests__/tableSortKeyboard.test.tsx checks the components).
 */
import type { KeyboardEvent } from 'react';
import type { ColumnsType } from 'antd/es/table';
import { activateOnSpace } from './keyboard';

const clickHeader = activateOnSpace((el) => (el as HTMLElement).click());

export function withKeyboardSort<T>(columns: ColumnsType<T>): ColumnsType<T> {
  return columns.map((col) => {
    const withChildren =
      'children' in col && col.children ? { ...col, children: withKeyboardSort(col.children) } : col;
    if (!('sorter' in col) || !col.sorter) return withChildren;
    const own = col.onHeaderCell;
    return {
      ...withChildren,
      onHeaderCell: (c) => {
        const cell = own?.(c) ?? {};
        return {
          ...cell,
          onKeyDownCapture: (e: KeyboardEvent<HTMLElement>) => {
            clickHeader(e);
            cell.onKeyDownCapture?.(e);
          },
        };
      },
    };
  }) as ColumnsType<T>;
}
