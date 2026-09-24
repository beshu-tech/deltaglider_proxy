import type { ReactNode } from 'react';
import { Button, Dropdown, Modal } from 'antd';
import { EllipsisOutlined } from '@ant-design/icons';

/** What a destructive item asks before it runs. */
export interface RowActionConfirm {
  title: ReactNode;
  content?: ReactNode;
  okText: string;
}

interface RowActionBase {
  key: string;
  label: ReactNode;
  icon?: ReactNode;
  disabled?: boolean;
  onSelect: () => void | Promise<unknown>;
}

/**
 * A destructive item must say how it is confirmed: a `RowActionConfirm`
 * (this menu asks), or `'caller'` when `onSelect` already opens its own
 * confirmation (for example a dialog that shows what the action affects).
 */
export type RowAction =
  | (RowActionBase & { danger?: false })
  | (RowActionBase & { danger: true; confirm: RowActionConfirm | 'caller' });

interface Props {
  actions: RowAction[];
  /** Accessible name, e.g. "More actions for backend hetzner-fsn1". */
  label: string;
  loading?: boolean;
  disabled?: boolean;
}

/**
 * THE row-actions control for every admin list: a "⋯" button that opens a
 * menu. Destructive actions live here, behind a confirmation, instead of as
 * a red icon on every row.
 *
 * Keyboard: Enter/Space on "⋯" opens the menu and moves focus into it, the
 * arrow keys move, Enter runs the item, Escape closes. Clicks stop here, so
 * they never also activate the row (a React portal still bubbles to the
 * row's handlers); rows ignore descendant keys through `activateOnKey`.
 */
export default function RowActionsMenu({ actions, label, loading, disabled }: Props) {
  if (actions.length === 0) return null;
  const run = (a: RowAction) => {
    if (a.danger && a.confirm !== 'caller') {
      const c = a.confirm;
      Modal.confirm({
        title: c.title,
        content: c.content,
        okText: c.okText,
        okButtonProps: { danger: true },
        onOk: () => a.onSelect(),
      });
      return;
    }
    void a.onSelect();
  };
  return (
    <span style={{ display: 'inline-flex' }} onClick={(e) => e.stopPropagation()}>
      <Dropdown
        trigger={['click']}
        autoFocus
        disabled={disabled}
        menu={{
          items: actions.map((a) => ({
            key: a.key,
            label: a.label,
            icon: a.icon,
            danger: a.danger,
            disabled: a.disabled,
          })),
          onClick: ({ key, domEvent }) => {
            domEvent.stopPropagation();
            const a = actions.find((x) => x.key === key);
            if (a) run(a);
          },
        }}
      >
        <Button
          size="small"
          type="text"
          icon={<EllipsisOutlined />}
          loading={loading}
          disabled={disabled}
          title="More actions"
          aria-label={label}
        />
      </Dropdown>
    </span>
  );
}
