import type { ReactNode } from 'react';
import { useColors } from '../ThemeContext';

/**
 * A clickable pill. A real <button>, so it is focusable, activates with
 * Enter/Space and has a role: a clickable AntD <Tag> or <span> has none.
 */
export default function PillButton({ children, onClick, title, ariaLabel }: {
  children: ReactNode;
  onClick: () => void;
  title?: string;
  ariaLabel?: string;
}) {
  const c = useColors();
  return (
    <button
      type="button"
      className="dg-pill-button"
      onClick={onClick}
      title={title}
      aria-label={ariaLabel}
      style={{
        cursor: 'pointer',
        borderRadius: 10,
        fontSize: 12,
        padding: '2px 10px',
        margin: 0,
        border: `1px solid ${c.ACCENT_BLUE}55`,
        background: `${c.ACCENT_BLUE}14`,
        color: c.ACCENT_BLUE_LIGHT,
        fontFamily: 'var(--font-ui)',
        lineHeight: '20px',
      }}
    >
      {children}
    </button>
  );
}
