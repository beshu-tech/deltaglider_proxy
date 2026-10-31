import { useEffect, useRef, useState } from 'react';
import { confirmDialog } from '../confirmDialog';
import {
  BookOutlined,
  DownOutlined,
  EyeInvisibleOutlined,
  EyeOutlined,
  HomeOutlined,
  LogoutOutlined,
  MoonOutlined,
  SettingOutlined,
  SunOutlined,
  UpOutlined,
} from '@ant-design/icons';
import { useTheme } from '../ThemeContext';
import TruncatedName from './TruncatedName';
import { SPACE } from './shared-styles';

interface Props {
  identityLabel: string;
  /** Second header line: role and auth mode (see identitySummary.ts). */
  identityDetail?: string;
  onBrowserClick?: () => void;
  onSettingsClick?: () => void;
  onDocsClick?: () => void;
  onLogout?: () => void;
  showHidden?: boolean;
  onToggleHidden?: () => void;
  placement?: 'up' | 'down';
  compact?: boolean;
  avatarOnly?: boolean;
}

/** Keys whose native click on the trigger opens the menu from the keyboard. */
const KEYS_THAT_CLICK = new Set(['Enter', ' ']);

/** The focusable entries of the open menu, in order. */
function menuItems(root: HTMLElement | null): HTMLElement[] {
  return root ? Array.from(root.querySelectorAll<HTMLElement>('[role="menuitem"]:not([disabled])')) : [];
}

export default function AccountMenu({
  identityLabel,
  identityDetail,
  onBrowserClick,
  onSettingsClick,
  onDocsClick,
  onLogout,
  showHidden,
  onToggleHidden,
  placement = 'up',
  compact = false,
  avatarOnly = false,
}: Props) {
  const { isDark, toggleTheme } = useTheme();
  const [open, setOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const itemsRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  // Which item gets focus when the menu opens (ArrowUp on the trigger opens at the last).
  const focusOnOpenRef = useRef<'first' | 'last' | null>('first');
  const label = identityLabel.trim() || 'user';
  const avatarLetter = (() => {
    const ch = label.charAt(0);
    return /[a-z]/i.test(ch) ? ch.toUpperCase() : label.slice(0, 1);
  })();
  const iconStyle: React.CSSProperties = { fontSize: 16, width: 20, display: 'inline-flex', justifyContent: 'center' };

  useEffect(() => {
    if (!open) return;

    const onPointerDown = (event: PointerEvent) => {
      const target = event.target;
      if (target instanceof Node && menuRef.current?.contains(target)) return;
      setOpen(false);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setOpen(false);
        triggerRef.current?.focus();
      }
    };

    document.addEventListener('pointerdown', onPointerDown);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [open]);

  // Opening the menu moves focus to its first item, so the arrow keys work.
  useEffect(() => {
    if (!open) return;
    // A tap or a click opens the menu without moving focus: a focus moved by
    // script after a tap kept a ring on the avatar (D57). The keyboard
    // (Enter, Space, arrows) opens it with focus on an item, for the arrows.
    const where = focusOnOpenRef.current;
    focusOnOpenRef.current = 'first';
    if (where === null) return;
    const items = menuItems(itemsRef.current);
    (where === 'last' ? items[items.length - 1] : items[0])?.focus();
  }, [open]);

  const onMenuKeyDown = (event: React.KeyboardEvent) => {
    const items = menuItems(itemsRef.current);
    const i = items.indexOf(document.activeElement as HTMLElement);
    const next = { ArrowDown: i + 1, ArrowUp: i - 1, Home: 0, End: items.length - 1 }[event.key];
    if (next === undefined || items.length === 0) return;
    event.preventDefault();
    items[(next + items.length) % items.length].focus();
  };

  const onTriggerKeyDown = (event: React.KeyboardEvent) => {
    // The native click of Enter / Space opens the menu: focus goes to an item.
    if (KEYS_THAT_CLICK.has(event.key)) focusOnOpenRef.current = 'first';
    if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
    event.preventDefault();
    focusOnOpenRef.current = event.key === 'ArrowUp' ? 'last' : 'first';
    if (open) {
      const items = menuItems(itemsRef.current);
      (event.key === 'ArrowUp' ? items[items.length - 1] : items[0])?.focus();
    } else {
      setOpen(true);
    }
  };

  const close = () => setOpen(false);
  const confirmLogout = async () => {
    if (await confirmDialog({ title: 'Sign out?', content: 'This clears your credentials and returns to the sign-in screen.', okText: 'Sign out' })) {
      onLogout?.();
    }
  };

  return (
    <div
      ref={menuRef}
      className={[
        'account-menu-wrap',
        placement === 'down' ? 'account-menu-wrap--down' : 'account-menu-wrap--up',
        compact ? 'account-menu-wrap--compact' : '',
      ].filter(Boolean).join(' ')}
    >
      {open && (
        <div className="account-menu-panel">
          {/* Who is signed in, and how. Outside the menu: it is text, not an item. */}
          <div
            style={{ padding: `2px ${SPACE.xxs}px ${SPACE.sm}px`, marginBottom: SPACE.xs, borderBottom: '1px solid color-mix(in srgb, currentColor 14%, transparent)' }}
          >
            <TruncatedName text={label} style={{ fontWeight: 700, fontSize: 13 }} />
            {identityDetail && (
              <div style={{ fontSize: 11, opacity: 0.7, marginTop: 2, lineHeight: 1.4, wordBreak: 'break-word' }}>
                {identityDetail}
              </div>
            )}
          </div>
          <div ref={itemsRef} role="menu" aria-label="Account" onKeyDown={onMenuKeyDown}>
          <div className="account-menu-section account-menu-section--first" role="group" aria-label="Navigation">
            <div className="account-menu-section-label dg-eyebrow" aria-hidden>Navigation</div>
            {/* Each view passes no handler for itself, so the menu never
                offers the page you are already on. */}
            {onBrowserClick && (
              <button
                type="button"
                className="account-menu-item"
                role="menuitem"
                onClick={() => {
                  close();
                  onBrowserClick();
                }}
              >
                <HomeOutlined aria-hidden style={iconStyle} />
                <span>Browser</span>
              </button>
            )}
            {onSettingsClick && (
              <button
                type="button"
                className="account-menu-item"
                role="menuitem"
                onClick={() => {
                  close();
                  onSettingsClick();
                }}
              >
                <SettingOutlined aria-hidden style={iconStyle} />
                <span>Settings</span>
              </button>
            )}
            {onDocsClick && (
              <button
                type="button"
                className="account-menu-item"
                role="menuitem"
                onClick={() => {
                  close();
                  onDocsClick();
                }}
              >
                <BookOutlined aria-hidden style={iconStyle} />
                <span>Documentation</span>
              </button>
            )}
          </div>
          <div className="account-menu-section" role="group" aria-label="Quick actions">
            <div className="account-menu-section-label dg-eyebrow" aria-hidden>Quick actions</div>
            <button
              type="button"
              className="account-menu-item"
              role="menuitem"
              onClick={toggleTheme}
            >
              {isDark ? (
                <SunOutlined aria-hidden style={iconStyle} />
              ) : (
                <MoonOutlined aria-hidden style={iconStyle} />
              )}
              <span>{isDark ? 'Switch to light mode' : 'Switch to dark mode'}</span>
            </button>
            {onToggleHidden && (
              <button
                type="button"
                className="account-menu-item"
                role="menuitem"
                aria-pressed={showHidden === true}
                onClick={onToggleHidden}
              >
                {showHidden ? (
                  <EyeInvisibleOutlined aria-hidden style={iconStyle} />
                ) : (
                  <EyeOutlined aria-hidden style={iconStyle} />
                )}
                <span>{showHidden ? 'Hide system files' : 'Show system files'}</span>
              </button>
            )}
          </div>
          {onLogout && (
            <div className="account-menu-signout-section" role="group" aria-label="Account">
              <button
                type="button"
                className="account-menu-item"
                role="menuitem"
                onClick={() => {
                  close();
                  confirmLogout();
                }}
              >
                <LogoutOutlined aria-hidden style={iconStyle} />
                <span>Sign out</span>
              </button>
            </div>
          )}
          </div>
        </div>
      )}
      <button
        ref={triggerRef}
        type="button"
        className="account-menu-trigger"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Account menu: ${label}`}
        onPointerDown={() => {
          focusOnOpenRef.current = null;
        }}
        onClick={() => setOpen((v) => !v)}
        onKeyDown={onTriggerKeyDown}
      >
        <span className="account-menu-avatar" aria-hidden>
          {avatarLetter}
        </span>
        {!avatarOnly && (
          <>
            <span className="account-menu-name" title={label}>
              {label}
            </span>
            {open ? (
              <UpOutlined className="account-menu-chevron" aria-hidden />
            ) : (
              <DownOutlined className="account-menu-chevron" aria-hidden />
            )}
          </>
        )}
      </button>
    </div>
  );
}
