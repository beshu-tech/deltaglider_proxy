import { useRef, useState, useEffect } from 'react';
import { Layout, Space, Button, Input, theme } from 'antd';
import { MenuOutlined, SearchOutlined, CloseOutlined, ReloadOutlined, SunOutlined, MoonOutlined, EyeOutlined, EyeInvisibleOutlined } from '@ant-design/icons';
import Breadcrumb from './Breadcrumb';
import { useColors, useTheme } from '../ThemeContext';

const { Header } = Layout;

interface Props {
  prefix: string;
  onNavigate: (prefix: string) => void;
  isMobile: boolean;
  onMenuClick: () => void;
  onRefresh: () => void;
  searchQuery: string;
  onSearchChange: (query: string) => void;
  refreshing: boolean;
  showHidden: boolean;
  onToggleHidden: () => void;
}

function SearchInput({
  inputRef,
  placeholder,
  value,
  onChange,
  onClose,
  size,
  style,
}: {
  inputRef: React.Ref<HTMLInputElement>;
  placeholder: string;
  value: string;
  onChange: (value: string) => void;
  onClose: () => void;
  size?: 'small' | 'middle' | 'large';
  style?: React.CSSProperties;
}) {
  const { TEXT_MUTED, BORDER, TEXT_PRIMARY } = useColors();
  return (
    <Input
      ref={inputRef as React.Ref<any>}
      placeholder={placeholder}
      aria-label="Filter objects and folders"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      prefix={<SearchOutlined aria-hidden="true" style={{ color: TEXT_MUTED }} />}
      suffix={
        <Button
          type="text"
          size="small"
          icon={<CloseOutlined />}
          aria-label="Close search"
          style={{ color: TEXT_MUTED, fontSize: 12 }}
          onClick={onClose}
        />
      }
      allowClear={false}
      size={size}
      style={{
        background: 'var(--input-bg)',
        borderColor: BORDER,
        color: TEXT_PRIMARY,
        borderRadius: 8,
        fontFamily: "var(--font-mono)",
        fontSize: 13,
        ...style,
      }}
    />
  );
}

export default function TopBar({ prefix, onNavigate, isMobile, onMenuClick, onRefresh, searchQuery, onSearchChange, refreshing, showHidden, onToggleHidden }: Props) {
  const { token } = theme.useToken();
  const { ACCENT_BLUE, TEXT_MUTED, BORDER } = useColors();
  const { isDark, toggleTheme } = useTheme();
  const [searchOpen, setSearchOpen] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (searchOpen) {
      setTimeout(() => inputRef.current?.focus(), 50);
    }
  }, [searchOpen]);

  const handleCloseSearch = () => {
    setSearchOpen(false);
    onSearchChange('');
  };

  return (
    <Header
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: isMobile ? '0 12px' : '0 20px',
        height: 52,
        lineHeight: '52px',
        background: token.colorBgContainer,
        borderBottom: `1px solid ${BORDER}`,
      }}
    >
      {/* Left: hamburger on mobile, breadcrumb or search on desktop */}
      <div style={{ flex: 1, minWidth: 0, display: 'flex', alignItems: 'center', gap: 12, paddingLeft: isMobile ? 0 : 40 }}>
        {isMobile && (
          <Button type="text" icon={<MenuOutlined />} onClick={onMenuClick} size="small" aria-label="Open navigation menu" />
        )}
        {!isMobile && (
          searchOpen ? (
            <SearchInput
              inputRef={inputRef}
              placeholder="Filter objects and folders..."
              value={searchQuery}
              onChange={onSearchChange}
              onClose={handleCloseSearch}
              style={{ maxWidth: 400 }}
            />
          ) : (
            <Breadcrumb prefix={prefix} onNavigate={onNavigate} />
          )
        )}
      </div>

      {/* Right: action icons */}
      <Space size={4} style={{ flexShrink: 0 }}>
        {isMobile && searchOpen ? (
          <SearchInput
            inputRef={inputRef}
            placeholder="Filter..."
            value={searchQuery}
            onChange={onSearchChange}
            onClose={handleCloseSearch}
            size="small"
            style={{ width: 160 }}
          />
        ) : (
          <Button
              type="text"
              icon={<SearchOutlined />}
              size="small"
              title="Search objects"
              aria-label="Search objects"
              style={{ color: searchOpen ? ACCENT_BLUE : TEXT_MUTED, transition: 'color 0.15s' }}
              onClick={() => setSearchOpen(!searchOpen)}
            />
        )}
        <Button
          type="text"
          icon={showHidden ? <EyeOutlined /> : <EyeInvisibleOutlined />}
          size="small"
          title={showHidden ? 'Hide system files' : 'Show system files'}
          onClick={onToggleHidden}
          aria-label={showHidden ? 'Hide system files' : 'Show system files'}
          style={{ color: showHidden ? ACCENT_BLUE : TEXT_MUTED, transition: 'color 0.15s' }}
        />
        {/* Divider between view toggles and utility actions */}
        <div style={{ width: 1, height: 20, background: BORDER, margin: '0 4px', flexShrink: 0 }} />
        <Button
          type="text"
          icon={<ReloadOutlined spin={refreshing} />}
          size="small"
          title="Refresh"
          onClick={onRefresh}
          aria-label="Refresh object list"
          style={{ color: TEXT_MUTED, transition: 'color 0.15s' }}
        />
        <Button
          type="text"
          icon={isDark ? <MoonOutlined /> : <SunOutlined />}
          size="small"
          title={isDark ? 'Switch to light mode' : 'Switch to dark mode'}
          onClick={toggleTheme}
          aria-label={isDark ? 'Switch to light mode' : 'Switch to dark mode'}
          style={{ color: TEXT_MUTED, transition: 'color 0.15s' }}
        />
      </Space>
    </Header>
  );
}
