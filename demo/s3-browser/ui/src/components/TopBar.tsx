import { Layout, Space, Tag, Button, Typography, theme } from 'antd';
import { BulbOutlined, BulbFilled, MenuOutlined } from '@ant-design/icons';
import Breadcrumb from './Breadcrumb';
import ConnectionSettings from './ConnectionSettings';

const { Header } = Layout;
const { Text } = Typography;

interface Props {
  prefix: string;
  onNavigate: (prefix: string) => void;
  connected: boolean;
  isDark: boolean;
  onToggleTheme: () => void;
  isMobile: boolean;
  onMenuClick: () => void;
}

export default function TopBar({ prefix, onNavigate, connected, isDark, onToggleTheme, isMobile, onMenuClick }: Props) {
  const { token } = theme.useToken();

  return (
    <Header
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: isMobile ? '0 12px' : '0 24px',
        height: 56,
        lineHeight: '56px',
        background: token.colorBgContainer,
        borderBottom: `1px solid ${token.colorBorderSecondary}`,
      }}
    >
      {/* Left: hamburger on mobile, logo + brand on desktop */}
      <Space align="center" size={isMobile ? 8 : 12} style={{ flexShrink: 0 }}>
        <Button type="text" icon={<MenuOutlined />} onClick={onMenuClick} />
        <div
          style={{
            width: 32,
            height: 32,
            borderRadius: 8,
            background: token.colorPrimary,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
          }}
        >
          <span style={{ color: '#fff', fontSize: 16, fontWeight: 700 }}>{'\u0394'}</span>
        </div>
        {!isMobile && (
          <Space size={8} align="center">
            <Text strong style={{ fontSize: 15 }}>
              <span style={{ color: token.colorPrimary }}>Delta</span>
              <span>Glider Proxy</span>
            </Text>
            <Tag style={{ marginInlineEnd: 0 }}>v0.1</Tag>
          </Space>
        )}
      </Space>

      {/* Breadcrumb (center) — hidden on mobile */}
      {!isMobile && (
        <div style={{ flex: 1, margin: '0 32px', minWidth: 0 }}>
          <Breadcrumb prefix={prefix} onNavigate={onNavigate} />
        </div>
      )}

      {/* Controls (right) */}
      <Space size={8} style={{ flexShrink: 0 }}>
        <Button
          type="text"
          icon={isDark ? <BulbFilled /> : <BulbOutlined />}
          onClick={onToggleTheme}
          title={isDark ? 'Switch to light mode' : 'Switch to dark mode'}
        />
        <ConnectionSettings connected={connected} isMobile={isMobile} />
      </Space>
    </Header>
  );
}
