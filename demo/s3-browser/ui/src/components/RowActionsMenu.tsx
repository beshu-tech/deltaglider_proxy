import { useRef, type ReactNode } from 'react';
import { Button, Modal } from 'antd';
import Dropdown from './KeyboardDropdown';
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
 * A destructive item must carry its confirmation; the menu asks it, so every
 * destructive action gets the same dialog and the same focus return.
 */
export type RowAction =
  | (RowActionBase & { danger?: false })
  | (RowActionBase & { danger: true; confirm: RowActionConfirm });

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
  // The confirm dialog returns focus to BODY when it closes; send it back to
  // "⋯" so a keyboard user keeps their place in the list.
  const buttonRef = useRef<HTMLButtonElement>(null);
  if (actions.length === 0) return null;
  const run = (a: RowAction) => {
    if (a.danger) {
      const c = a.confirm;
      Modal.confirm({
        title: c.title,
        content: c.content,
        okText: c.okText,
        okButtonProps: { danger: true },
        onOk: () => a.onSelect(),
        // The row may be gone (a deleted rule); then focus stays where it is.
        afterClose: () => buttonRef.current?.focus(),
      });
      return;
    }
    void a.onSelect();
  };
  return (
    <span style={{ display: 'inline-flex' }} onClick={(e) => e.stopPropagation()}>
      <Dropdown
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
          ref={buttonRef}
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
