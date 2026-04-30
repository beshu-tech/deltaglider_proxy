import { useEffect, useRef, useState } from 'react';
import {
  BookOutlined,
  CopyOutlined,
  DownloadOutlined,
  DownOutlined,
  EyeInvisibleOutlined,
  EyeOutlined,
  FileTextOutlined,
  HomeOutlined,
  ImportOutlined,
  LogoutOutlined,
  MoonOutlined,
  SettingOutlined,
  SunOutlined,
  UploadOutlined,
  UpOutlined,
} from '@ant-design/icons';
import { useTheme } from '../ThemeContext';
import type { SectionName } from '../adminApi';
import { SectionYamlModal } from './CopySectionYamlButton';

export interface AccountMenuConfigProps {
  configSection?: SectionName;
  onShowFullConfigYaml?: () => void;
  onImportFullConfigYaml?: () => void;
  onExportFullBackup?: () => void;
  onImportFullBackup?: () => void;
}

interface Props extends AccountMenuConfigProps {
  identityLabel: string;
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

export default function AccountMenu({
  identityLabel,
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
  onExportFullBackup,
  onImportFullBackup,
}: Props) {
  const { isDark, toggleTheme } = useTheme();
  const [open, setOpen] = useState(false);
  const [sectionYamlOpen, setSectionYamlOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
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
      if (event.key === 'Escape') setOpen(false);
    };

    document.addEventListener('pointerdown', onPointerDown);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [open]);

  useEffect(() => {
    if (!configSection) setSectionYamlOpen(false);
  }, [configSection]);

  const close = () => setOpen(false);
  const hasConfigActions = Boolean(configSection || onShowFullConfigYaml || onImportFullConfigYaml);
  const hasBackupActions = Boolean(onExportFullBackup || onImportFullBackup);
  const configLabel = configSection
    ? `${configSection.charAt(0).toUpperCase()}${configSection.slice(1)} section YAML`
    : 'Section YAML';
  const settingsHelp = 'Runtime configuration only; excludes the encrypted IAM DB and full recovery material.';
  const recoveryHelp = 'Restore bundle including IAM/control-plane state and restore metadata.';
  const confirmLogout = () => {
    if (window.confirm('Sign out? This will clear your credentials and return to the login screen.')) {
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
        <div className="account-menu-panel" role="menu">
          <div className="account-menu-section account-menu-section--first" role="group" aria-label="Navigation">
            <div className="account-menu-section-label">Navigation</div>
            <button
              type="button"
              className="account-menu-item"
              role="menuitem"
              onClick={() => {
                close();
                onBrowserClick?.();
              }}
            >
              <HomeOutlined aria-hidden style={iconStyle} />
              <span>Browser</span>
            </button>
            {canAdmin === true && onSettingsClick && (
              <button
                type="button"
                className="account-menu-item"
                role="menuitem"
                onClick={() => {
                  close();
                  onSettingsClick?.();
                }}
              >
                <SettingOutlined aria-hidden style={iconStyle} />
                <span>Settings</span>
              </button>
            )}
            <button
              type="button"
              className="account-menu-item"
              role="menuitem"
              onClick={() => {
                close();
                onDocsClick?.();
              }}
            >
              <BookOutlined aria-hidden style={iconStyle} />
              <span>Documentation</span>
            </button>
          </div>
          {hasConfigActions && (
            <div className="account-menu-section" role="group" aria-label="Settings" title={settingsHelp}>
              <div className="account-menu-section-label">Settings</div>
              <div className="account-menu-section-help">{settingsHelp}</div>
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
            </div>
          )}
          {hasBackupActions && (
            <div
              className="account-menu-section"
              role="group"
              aria-label="Recovery"
              title={recoveryHelp}
            >
              <div className="account-menu-section-label">Recovery</div>
              <div className="account-menu-section-help">{recoveryHelp}</div>
              {onExportFullBackup && (
                <button
                  type="button"
                  className="account-menu-item"
                  role="menuitem"
                  title={recoveryHelp}
                  onClick={() => {
                    close();
                    onExportFullBackup();
                  }}
                >
                  <DownloadOutlined aria-hidden style={iconStyle} />
                  <span>Download recovery backup</span>
                </button>
              )}
              {onImportFullBackup && (
                <button
                  type="button"
                  className="account-menu-item"
                  role="menuitem"
                  title={recoveryHelp}
                  onClick={() => {
                    close();
                    onImportFullBackup();
                  }}
                >
                  <UploadOutlined aria-hidden style={iconStyle} />
                  <span>Restore recovery backup</span>
                </button>
              )}
            </div>
          )}
          <div className="account-menu-section" role="group" aria-label="Quick actions">
            <div className="account-menu-section-label">Quick actions</div>
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
      )}
      <button
        type="button"
        className="account-menu-trigger"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Account menu: ${label}`}
        onClick={() => setOpen((v) => !v)}
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
