import { useState } from 'react';
import { Button, Input, Typography, Space, Alert } from 'antd';
import { SaveOutlined, ApiOutlined, CloudOutlined } from '@ant-design/icons';
import { getEndpoint, setEndpoint, getRegion, setRegion, getCredentials, setCredentials, testConnection } from '../s3client';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';

const { Text } = Typography;

export default function BrowserConnectionCard() {
  const { cardStyle, labelStyle, inputRadius } = useCardStyles();

  const [endpoint, setConnEndpoint] = useState(getEndpoint());
  const [region, setConnRegion] = useState(getRegion());
  const savedCreds = getCredentials();
  const [accessKey, setConnAccessKey] = useState(savedCreds.accessKeyId);
  const [secretKey, setConnSecretKey] = useState(savedCreds.secretAccessKey);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ ok: boolean; buckets?: string[]; error?: string } | null>(null);

  return (
    <form onSubmit={(e) => e.preventDefault()} style={cardStyle}>
      <SectionHeader icon={<CloudOutlined />} title="Browser Connection" />
      <Text type="secondary" style={{ fontSize: 12, fontFamily: "var(--font-ui)", display: 'block', marginBottom: 12 }}>
        How this browser connects to the DeltaGlider proxy.
      </Text>

      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Proxy Endpoint</span>
        <Input
          value={endpoint}
          onChange={(e) => setConnEndpoint(e.target.value)}
          placeholder="http://localhost:9000"
          style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
        />
      </div>

      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Region</span>
        <Input
          value={region}
          onChange={(e) => setConnRegion(e.target.value)}
          placeholder="us-east-1"
          style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
        />
      </div>

      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Access Key ID</span>
        <Input
          value={accessKey}
          onChange={(e) => setConnAccessKey(e.target.value)}
          placeholder="minioadmin"
          autoComplete="username"
          style={{ ...inputRadius, fontFamily: "var(--font-mono)", fontSize: 13 }}
        />
      </div>

      <div style={{ marginTop: 12 }}>
        <span style={labelStyle}>Secret Access Key</span>
        <Input.Password
          value={secretKey}
          onChange={(e) => setConnSecretKey(e.target.value)}
          placeholder="minioadmin"
          autoComplete="current-password"
          style={{ ...inputRadius }}
        />
      </div>

      <Space style={{ width: '100%', marginTop: 16 }} size={8}>
        <Button
          icon={<ApiOutlined />}
          loading={testing}
          onClick={async () => {
            setTesting(true);
            setTestResult(null);
            const result = await testConnection(endpoint, accessKey, secretKey, region);
            setTestResult(result);
            setTesting(false);
          }}
          style={{ borderRadius: 8, fontFamily: "var(--font-ui)", fontWeight: 600 }}
        >
          Test
        </Button>
        <Button
          type="primary"
          icon={<SaveOutlined />}
          onClick={() => {
            setEndpoint(endpoint);
            setRegion(region);
            setCredentials(accessKey, secretKey);
            setTestResult({ ok: true, buckets: [] });
            window.location.reload();
          }}
          style={{ borderRadius: 8, fontFamily: "var(--font-ui)", fontWeight: 600 }}
        >
          Save &amp; Reconnect
        </Button>
      </Space>

      {testResult && (
        <Alert
          style={{ marginTop: 12, borderRadius: 8 }}
          type={testResult.ok ? 'success' : 'error'}
          showIcon
          message={
            testResult.ok
              ? `Connected — ${testResult.buckets?.length ?? 0} bucket${(testResult.buckets?.length ?? 0) === 1 ? '' : 's'} found`
              : 'Connection failed'
          }
          description={testResult.ok ? testResult.buckets?.join(', ') : testResult.error}
        />
      )}
    </form>
  );
}
