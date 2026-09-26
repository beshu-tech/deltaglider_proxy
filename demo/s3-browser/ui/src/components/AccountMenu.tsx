import { useEffect, useId, useRef, useState } from 'react';
import { confirmDialog } from '../confirmDialog';
import {
  BookOutlined,
  CopyOutlined,
  DownOutlined,
  EyeInvisibleOutlined,
  EyeOutlined,
  FileTextOutlined,
  HomeOutlined,
  ImportOutlined,
  LogoutOutlined,
  MoonOutlined,
  SafetyCertificateOutlined,
  SettingOutlined,
  SunOutlined,
  TeamOutlined,
  UpOutlined,
} from '@ant-design/icons';
import { useTheme } from '../ThemeContext';
import type { SectionName } from '../adminApi';
import { SectionYamlModal } from './CopySectionYamlButton';

export interface AccountMenuConfigProps {
  configSection?: SectionName;
  onShowFullConfigYaml?: () => void;
  onImportFullConfigYaml?: () => void;
  onExportFullIam?: () => void;
  onImportFullIam?: () => void;
}

interface Props extends AccountMenuConfigProps {
  identityLabel: string;
  /** Second header line: role and auth mode (see identitySummary.ts). */
  identityDetail?: string;
  canAdmin?: boolean;
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

/** The focusable entries of the open menu, in order. */
function menuItems(root: HTMLElement | null): HTMLElement[] {
  return root ? Array.from(root.querySelectorAll<HTMLElement>('[role="menuitem"]:not([disabled])')) : [];
}

export default function AccountMenu({
  identityLabel,
  identityDetail,
  canAdmin,
  onBrowserClick,
  onSettingsClick,
  onDocsClick,
  onLogout,
  showHidden,
  onToggleHidden,
  placement = 'up',
  compact = false,
  avatarOnly = false,
  configSection,
  onShowFullConfigYaml,
  onImportFullConfigYaml,
  onExportFullIam,
  onImportFullIam,
}: Props) {
  const { isDark, toggleTheme } = useTheme();
  const [open, setOpen] = useState(false);
  const [sectionYamlOpen, setSectionYamlOpen] = useState(false);
  const idBase = useId();
  const menuRef = useRef<HTMLDivElement>(null);
  const itemsRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  // Which item gets focus when the menu opens (ArrowUp on the trigger opens at the last).
  const focusOnOpenRef = useRef<'first' | 'last'>('first');
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
    const items = menuItems(itemsRef.current);
    (focusOnOpenRef.current === 'last' ? items[items.length - 1] : items[0])?.focus();
    focusOnOpenRef.current = 'first';
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

  useEffect(() => {
    if (!configSection) setSectionYamlOpen(false);
  }, [configSection]);

  const close = () => setOpen(false);
  const isAdmin = canAdmin === true;
  const hasConfigActions =
    isAdmin &&
    Boolean(
      configSection ||
        onShowFullConfigYaml ||
        onImportFullConfigYaml ||
        onExportFullIam ||
        onImportFullIam
    );
  const configLabel = configSection
    ? `${configSection.charAt(0).toUpperCase()}${configSection.slice(1)} section YAML`
    : 'Section YAML';
  const settingsHelp = 'Just your settings — does not include users/groups or full backup bundles.';
  const iamHelp = 'Full IAM (users, groups, providers, rules). Export includes LIVE secrets — handle like a password file.';
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
            style={{ padding: '2px 4px 10px', marginBottom: 8, borderBottom: '1px solid color-mix(in srgb, currentColor 14%, transparent)' }}
          >
            <div style={{ fontWeight: 700, fontSize: 13, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={label}>
              {label}
            </div>
            {identityDetail && (
              <div style={{ fontSize: 11, opacity: 0.7, marginTop: 2, lineHeight: 1.4, wordBreak: 'break-word' }}>
                {identityDetail}
              </div>
            )}
          </div>
          <div ref={itemsRef} role="menu" aria-label="Account" onKeyDown={onMenuKeyDown}>
          <div className="account-menu-section account-menu-section--first" role="group" aria-label="Navigation">
            <div className="account-menu-section-label" aria-hidden>Navigation</div>
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
          {hasConfigActions && (
            <div className="account-menu-section" role="group" aria-label="Settings" aria-describedby={`${idBase}-settings-help`} title={settingsHelp}>
              <div className="account-menu-section-label" aria-hidden>Settings</div>
              <div className="account-menu-section-help" id={`${idBase}-settings-help`} aria-hidden>{settingsHelp}</div>
              {configSection && (
                <button
                  type="button"
                  className="account-menu-item"
                  role="menuitem"
                  title={settingsHelp}
                  onClick={() => {
                    close();
                    setSectionYamlOpen(true);
                  }}
                >
                  <CopyOutlined aria-hidden style={iconStyle} />
                  <span>{configLabel}</span>
                </button>
              )}
              {onShowFullConfigYaml && (
                <button
                  type="button"
                  className="account-menu-item"
                  role="menuitem"
                  title={settingsHelp}
                  onClick={() => {
                    close();
                    onShowFullConfigYaml();
                  }}
                >
                  <FileTextOutlined aria-hidden style={iconStyle} />
                  <span>Export settings YAML</span>
                </button>
              )}
              {onImportFullConfigYaml && (
                <button
                  type="button"
                  className="account-menu-item"
                  role="menuitem"
                  title={settingsHelp}
                  onClick={() => {
                    close();
                    onImportFullConfigYaml();
                  }}
                >
                  <ImportOutlined aria-hidden style={iconStyle} />
                  <span>Import settings YAML</span>
                </button>
              )}
              {(onExportFullIam || onImportFullIam) && (
                <div className="account-menu-section-help" title={iamHelp} aria-hidden>{iamHelp}</div>
              )}
              {onExportFullIam && (
                <button
                  type="button"
                  className="account-menu-item"
                  role="menuitem"
                  title={iamHelp}
                  onClick={() => {
                    close();
                    onExportFullIam();
                  }}
                >
                  <SafetyCertificateOutlined aria-hidden style={iconStyle} />
                  <span>Export full IAM (YAML)</span>
                </button>
              )}
              {onImportFullIam && (
                <button
                  type="button"
                  className="account-menu-item"
                  role="menuitem"
                  title={iamHelp}
                  onClick={() => {
                    close();
                    onImportFullIam();
                  }}
                >
                  <TeamOutlined aria-hidden style={iconStyle} />
                  <span>Import full IAM (YAML)</span>
                </button>
              )}
            </div>
          )}
          <div className="account-menu-section" role="group" aria-label="Quick actions">
            <div className="account-menu-section-label" aria-hidden>Quick actions</div>
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
                className="account-menu-item account-menu-item--danger"
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
      <SectionYamlModal
        section={configSection}
        open={sectionYamlOpen}
        onClose={() => setSectionYamlOpen(false)}
      />
    </div>
  );
}
