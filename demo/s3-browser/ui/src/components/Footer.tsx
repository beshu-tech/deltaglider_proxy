import { Layout, Space, Typography, Badge, theme } from 'antd';
import { GithubOutlined, XOutlined } from '@ant-design/icons';
import { getEndpoint, getBucket } from '../s3client';

const { Footer: AntFooter } = Layout;
const { Text, Link } = Typography;

interface Props {
  connected: boolean;
  objectCount: number;
  isMobile: boolean;
}

export default function Footer({ connected, objectCount, isMobile }: Props) {
  const endpoint = getEndpoint();
  const shortEndpoint = endpoint.length > 40 ? endpoint.slice(0, 37) + '...' : endpoint;
  const { token } = theme.useToken();

  if (isMobile) {
    return (
      <AntFooter
        style={{
          padding: '12px 16px',
          borderTop: `1px solid ${token.colorBorderSecondary}`,
          background: token.colorBgContainer,
        }}
      >
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8, fontSize: 12 }}>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <Space size={6}>
              <Badge status={connected ? 'success' : 'error'} />
              <Text type="secondary" code style={{ fontSize: 11 }}>{getBucket()}</Text>
              <Text type="secondary" style={{ fontSize: 11 }}>{objectCount} obj</Text>
            </Space>
            <Space size={12}>
              <Link href="https://github.com/beshu-tech/deltaglider" target="_blank" style={{ fontSize: 12 }}>
                <GithubOutlined />
              </Link>
              <Link href="https://x.com/s_scarduzio" target="_blank" style={{ fontSize: 12 }}>
                <XOutlined />
              </Link>
            </Space>
          </div>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <Space size={8}>
              <Link href="https://beshu.tech/" target="_blank" style={{ fontWeight: 600, fontSize: 11 }}>
                Beshu Limited
              </Link>
              <Text type="secondary" style={{ fontSize: 11 }}>GPL-3.0</Text>
            </Space>
            <Text strong style={{ fontSize: 11, color: token.colorPrimary }}>DeltaGlider</Text>
          </div>
        </div>
      </AntFooter>
    );
  }

  return (
    <AntFooter
      style={{
        padding: '8px 24px',
        borderTop: `1px solid ${token.colorBorderSecondary}`,
        background: token.colorBgContainer,
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', fontSize: 12 }}>
        {/* Left: company + products */}
        <Space size={12}>
          <Link href="https://beshu.tech/" target="_blank" style={{ fontWeight: 600, fontSize: 12 }}>
            Beshu Limited
          </Link>
          <Text type="secondary">|</Text>
          <Space size={12}>
            <Link href="https://readonlyrest.com" target="_blank" style={{ fontSize: 12 }}>ReadonlyREST</Link>
            <Link href="https://anaphora.it" target="_blank" style={{ fontSize: 12 }}>Anaphora</Link>
            <Text strong style={{ fontSize: 12, color: token.colorPrimary }}>DeltaGlider</Text>
          </Space>
        </Space>

        {/* Center: connection status */}
        <Space size={8}>
          <Badge status={connected ? 'success' : 'error'} />
          <Text type="secondary" code style={{ fontSize: 11 }}>{shortEndpoint}</Text>
          <Text type="secondary">|</Text>
          <Text type="secondary" code style={{ fontSize: 11 }}>{getBucket()}</Text>
          <Text type="secondary" style={{ fontSize: 11 }}>{objectCount} obj</Text>
        </Space>

        {/* Right: links + license */}
        <Space size={12}>
          <Link href="https://github.com/beshu-tech/deltaglider" target="_blank" style={{ fontSize: 12 }}>
            <GithubOutlined style={{ marginRight: 4 }} />GitHub
          </Link>
          <Link href="https://x.com/s_scarduzio" target="_blank" style={{ fontSize: 12 }}>
            <XOutlined style={{ marginRight: 4 }} />@s_scarduzio
          </Link>
          <Text type="secondary">|</Text>
          <Text type="secondary" style={{ fontSize: 12 }}>GPL-3.0</Text>
        </Space>
      </div>
    </AntFooter>
  );
}
