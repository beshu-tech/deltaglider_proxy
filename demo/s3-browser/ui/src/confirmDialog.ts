import type { ReactNode } from 'react';
import { Modal } from 'antd';

/**
 * THE yes/no confirmation (in place of window.confirm, which blocks the page,
 * ignores the theme, and has no focus management). Resolves true on OK and
 * false on Cancel, Escape or the close icon. The dialog returns focus to the
 * element that opened it.
 */
export function confirmDialog(opts: {
  title: ReactNode;
  content?: ReactNode;
  okText?: string;
  cancelText?: string;
  danger?: boolean;
}): Promise<boolean> {
  return new Promise((resolve) => {
    Modal.confirm({
      title: opts.title,
      content: opts.content,
      okText: opts.okText ?? 'OK',
      cancelText: opts.cancelText ?? 'Cancel',
      okButtonProps: opts.danger ? { danger: true } : undefined,
      onOk: () => resolve(true),
      onCancel: () => resolve(false),
    });
  });
}
