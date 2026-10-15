import { createElement } from 'react';
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

/**
 * The bulk-delete confirmation, after the selection is counted: resolves
 * `true` on the OK button, `false` on Cancel or close.
 */
export function confirmDeletePlan(c: { title: string; body: string[]; okText: string }): Promise<boolean> {
  return new Promise((resolve) => {
    Modal.confirm({
      title: c.title,
      content: c.body.map((text, i) => createElement('p', { key: i, style: { margin: i ? '8px 0 0' : 0 } }, text)),
      okText: c.okText,
      okButtonProps: { danger: true },
      onOk: () => resolve(true),
      onCancel: () => resolve(false),
    });
  });
}
