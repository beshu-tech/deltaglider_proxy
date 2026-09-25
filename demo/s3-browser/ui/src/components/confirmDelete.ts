import { Modal } from 'antd';

/**
 * THE delete confirmation: one dialog shape for every permanent delete of
 * objects (bulk bar, inspector). `onOk` may return a promise; the dialog
 * keeps its OK button loading until it settles.
 */
export function confirmDelete(content: string, onOk: () => unknown): void {
  Modal.confirm({
    title: 'Delete permanently?',
    content,
    okText: 'Delete',
    okButtonProps: { danger: true },
    onOk,
  });
}
