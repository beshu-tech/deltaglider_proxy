import { cloneElement, isValidElement, useId, useRef, useState } from 'react';
// The one sanctioned import of AntD's Dropdown (see eslint.config.mjs).
// eslint-disable-next-line no-restricted-imports
import { Dropdown, type DropdownProps } from 'antd';

/**
 * AntD's Dropdown, with the menu focused when it opens: a keyboard user who
 * opens it with Enter can then move through the items with the arrow keys.
 * AntD's own `autoFocus` does nothing in AntD 6.4 (its overlay wrapper does
 * not forward the ref it focuses). The trigger defaults to click: a hover
 * menu cannot be opened from the keyboard at all. The trigger carries
 * `aria-haspopup="menu"` and `aria-expanded`, which AntD does not set, so a
 * screen reader announces a menu button.
 */
export default function KeyboardDropdown({ children, onOpenChange, rootClassName, trigger = ['click'], ...props }: DropdownProps) {
  const cls = `kbd-dropdown-${useId().replace(/[^a-zA-Z0-9_-]/g, '')}`;
  const frame = useRef(0);
  const [openState, setOpenState] = useState(false);
  const isOpen = props.open ?? openState;
  return (
    <Dropdown
      {...props}
      trigger={trigger}
      rootClassName={rootClassName ? `${rootClassName} ${cls}` : cls}
      onOpenChange={(open, info) => {
        setOpenState(open);
        onOpenChange?.(open, info);
        cancelAnimationFrame(frame.current);
        if (!open) return;
        // The popup mounts on the next frames; stop once the menu has focus.
        let tries = 0;
        const focusMenu = () => {
          const menu = document.querySelector<HTMLElement>(`.${cls} [role="menu"]`);
          if (menu) menu.focus();
          else if (tries++ < 10) frame.current = requestAnimationFrame(focusMenu);
        };
        frame.current = requestAnimationFrame(focusMenu);
      }}
    >
      {isValidElement<Record<string, unknown>>(children)
        ? cloneElement(children, { 'aria-haspopup': 'menu', 'aria-expanded': isOpen })
        : children}
    </Dropdown>
  );
}
