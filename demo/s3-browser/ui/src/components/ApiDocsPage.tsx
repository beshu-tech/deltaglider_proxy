import { useState } from 'react';
import { Typography, Button, Tag, Input, Alert, Space } from 'antd';
import { SendOutlined, CopyOutlined, ArrowLeftOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';

const { Text, Title } = Typography;
const { TextArea } = Input;

interface Endpoint {
  method: 'GET' | 'POST' | 'PUT' | 'DELETE' | 'HEAD';
  path: string;
  summary: string;
  description?: string;
  auth: 'admin' | 'sigv4' | 'none';
  body?: string; // example JSON body
  response?: string; // example response
}

const METHOD_COLORS: Record<string, { text: string; bg: string; border: string }> = {
  GET:    { text: '#4ade80', bg: 'rgba(74, 222, 128, 0.10)', border: 'rgba(74, 222, 128, 0.25)' },
  POST:   { text: '#60a5fa', bg: 'rgba(96, 165, 250, 0.10)', border: 'rgba(96, 165, 250, 0.25)' },
  PUT:    { text: '#fbbf24', bg: 'rgba(251, 191, 36, 0.10)', border: 'rgba(251, 191, 36, 0.25)' },
  DELETE: { text: '#fb7185', bg: 'rgba(251, 113, 133, 0.10)', border: 'rgba(251, 113, 133, 0.25)' },
  HEAD:   { text: '#a78bfa', bg: 'rgba(167, 139, 250, 0.10)', border: 'rgba(167, 139, 250, 0.25)' },
};

const ADMIN_API: Endpoint[] = [
  { method: 'POST', path: '/api/admin/login', summary: 'Admin login', auth: 'none',
    body: '{"password": "your-admin-password"}',
    response: '{"ok": true}' },
  { method: 'POST', path: '/api/admin/logout', summary: 'Admin logout', auth: 'admin' },
  { method: 'GET', path: '/api/admin/session', summary: 'Check session validity', auth: 'admin',
    response: '{"valid": true}' },
  { method: 'GET', path: '/api/admin/config', summary: 'Get proxy configuration', auth: 'admin',
    response: '{"listen_addr": "0.0.0.0:9000", "backend_type": "s3", ...}' },
  { method: 'PUT', path: '/api/admin/config', summary: 'Update proxy configuration', auth: 'admin',
    body: '{"max_delta_ratio": 0.5}',
    response: '{"success": true, "warnings": [], "requires_restart": false}' },
  { method: 'PUT', path: '/api/admin/password', summary: 'Change bootstrap password', auth: 'admin',
    body: '{"current_password": "old", "new_password": "new"}' },
  { method: 'POST', path: '/api/admin/test-s3', summary: 'Test S3 backend connectivity', auth: 'admin',
    body: '{"endpoint": "https://s3.amazonaws.com", "region": "us-east-1"}',
    response: '{"success": true, "buckets": ["my-bucket"]}' },
];

const IAM_API: Endpoint[] = [
  { method: 'GET', path: '/api/admin/users', summary: 'List all IAM users', auth: 'admin',
    description: 'Returns all users with masked secrets.',
    response: '[{"id": 1, "name": "admin", "access_key_id": "admin@co.com", "enabled": true, "permissions": [...]}]' },
  { method: 'POST', path: '/api/admin/users', summary: 'Create IAM user', auth: 'admin',
    description: 'Returns the full user including the secret (shown once).',
    body: '{"name": "ci-bot", "access_key_id": "ci@co.com", "secret_access_key": "optional", "enabled": true, "permissions": [{"actions": ["read", "write"], "resources": ["releases/*"]}]}',
    response: '{"id": 2, "name": "ci-bot", "access_key_id": "ci@co.com", "secret_access_key": "abc123...", ...}' },
  { method: 'PUT', path: '/api/admin/users/:id', summary: 'Update IAM user', auth: 'admin',
    body: '{"name": "new-name", "enabled": false, "permissions": [...]}' },
  { method: 'DELETE', path: '/api/admin/users/:id', summary: 'Delete IAM user', auth: 'admin',
    description: 'Cascade-deletes all permissions. Returns 204.' },
  { method: 'POST', path: '/api/admin/users/:id/rotate-keys', summary: 'Rotate access keys', auth: 'admin',
    description: 'Auto-generates new keys, or accepts custom values.',
    body: '{"access_key_id": "new-key", "secret_access_key": "new-secret"}',
    response: '{"id": 2, "access_key_id": "new-key", "secret_access_key": "new-secret", ...}' },
];

const S3_API: Endpoint[] = [
  { method: 'GET', path: '/', summary: 'List buckets', auth: 'sigv4' },
  { method: 'PUT', path: '/{bucket}', summary: 'Create bucket', auth: 'sigv4' },
  { method: 'HEAD', path: '/{bucket}', summary: 'Head bucket', auth: 'sigv4' },
  { method: 'DELETE', path: '/{bucket}', summary: 'Delete bucket', auth: 'sigv4' },
  { method: 'GET', path: '/{bucket}?list-type=2', summary: 'List objects (v2)', auth: 'sigv4' },
  { method: 'PUT', path: '/{bucket}/{key}', summary: 'Put object', auth: 'sigv4',
    description: 'Supports x-amz-copy-source for CopyObject. Delta compression applied automatically for eligible file types.' },
  { method: 'GET', path: '/{bucket}/{key}', summary: 'Get object', auth: 'sigv4',
    description: 'Delta-compressed files are reconstructed transparently.' },
  { method: 'HEAD', path: '/{bucket}/{key}', summary: 'Head object', auth: 'sigv4' },
  { method: 'DELETE', path: '/{bucket}/{key}', summary: 'Delete object', auth: 'sigv4' },
  { method: 'POST', path: '/{bucket}?delete', summary: 'Delete multiple objects', auth: 'sigv4' },
  { method: 'POST', path: '/{bucket}/{key}?uploads', summary: 'Create multipart upload', auth: 'sigv4' },
  { method: 'PUT', path: '/{bucket}/{key}?partNumber=N&uploadId=X', summary: 'Upload part', auth: 'sigv4' },
  { method: 'POST', path: '/{bucket}/{key}?uploadId=X', summary: 'Complete multipart upload', auth: 'sigv4' },
  { method: 'DELETE', path: '/{bucket}/{key}?uploadId=X', summary: 'Abort multipart upload', auth: 'sigv4' },
];

const OPS_API: Endpoint[] = [
  { method: 'GET', path: '/health', summary: 'Health check', auth: 'none',
    response: '{"status": "healthy", "backend": "ready", "cache_entries": 0, ...}' },
  { method: 'GET', path: '/stats', summary: 'Storage statistics', auth: 'none',
    description: 'Returns per-bucket storage stats. Cached for 10s.' },
  { method: 'GET', path: '/metrics', summary: 'Prometheus metrics', auth: 'none',
    description: 'OpenMetrics format for Prometheus scraping.' },
];

function EndpointCard({ ep }: { ep: Endpoint }) {
  const colors = useColors();
  const [expanded, setExpanded] = useState(false);
  const [response, setResponse] = useState('');
  const [loading, setLoading] = useState(false);
  const [body, setBody] = useState(ep.body ?? '');

  const tryIt = async () => {
    setLoading(true);
    setResponse('');
    try {
      const opts: RequestInit = {
        method: ep.method,
        credentials: 'include',
      };
      if (body && (ep.method === 'POST' || ep.method === 'PUT')) {
        opts.headers = { 'Content-Type': 'application/json' };
        opts.body = body;
      }
      const res = await fetch(ep.path, opts);
      const text = await res.text();
      try {
        setResponse(JSON.stringify(JSON.parse(text), null, 2));
      } catch {
        setResponse(text.substring(0, 2000));
      }
    } catch (e) {
      setResponse(`Error: ${e instanceof Error ? e.message : 'Request failed'}`);
    } finally {
      setLoading(false);
    }
  };

  const authTag = ep.auth === 'admin'
    ? <Tag color="gold" style={{ margin: 0 }}>Admin Session</Tag>
    : ep.auth === 'sigv4'
    ? <Tag color="blue" style={{ margin: 0 }}>SigV4</Tag>
    : <Tag style={{ margin: 0 }}>Public</Tag>;

  return (
    <div style={{
      border: `1px solid ${colors.BORDER}`,
      borderRadius: 8,
      marginBottom: 8,
      overflow: 'hidden',
    }}>
      <div
        onClick={() => setExpanded(!expanded)}
        style={{
          padding: '10px 16px',
          cursor: 'pointer',
          display: 'flex',
          alignItems: 'center',
          gap: 12,
          background: expanded ? colors.BG_BASE : 'transparent',
        }}
      >
        <span style={{
          margin: 0, fontWeight: 700, minWidth: 60, textAlign: 'center', display: 'inline-block',
          padding: '2px 10px', borderRadius: 6, fontSize: 12, fontFamily: 'var(--font-mono)',
          color: METHOD_COLORS[ep.method]?.text || '#ccc',
          background: METHOD_COLORS[ep.method]?.bg || 'transparent',
          border: `1px solid ${METHOD_COLORS[ep.method]?.border || 'transparent'}`,
        }}>
          {ep.method}
        </span>
        <Text code style={{ fontFamily: 'var(--font-mono)', fontSize: 13, flex: 1 }}>{ep.path}</Text>
        <Text type="secondary" style={{ fontSize: 12 }}>{ep.summary}</Text>
        {authTag}
      </div>

      {expanded && (
        <div style={{ padding: 16, borderTop: `1px solid ${colors.BORDER}`, background: colors.BG_BASE }}>
          {ep.description && (
            <Text type="secondary" style={{ display: 'block', marginBottom: 12, fontSize: 13 }}>{ep.description}</Text>
          )}

          {ep.body && (
            <div style={{ marginBottom: 12 }}>
              <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', fontWeight: 600 }}>Request Body</Text>
              <TextArea
                value={body}
                onChange={e => setBody(e.target.value)}
                autoSize={{ minRows: 2, maxRows: 8 }}
                style={{ fontFamily: 'var(--font-mono)', fontSize: 12, borderRadius: 6, marginTop: 4 }}
              />
            </div>
          )}

          {ep.response && !response && (
            <div style={{ marginBottom: 12 }}>
              <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', fontWeight: 600 }}>Example Response</Text>
              <pre style={{
                background: colors.BG_CARD,
                border: `1px solid ${colors.BORDER}`,
                borderRadius: 6,
                padding: 12,
                margin: '4px 0 0',
                fontFamily: 'var(--font-mono)',
                fontSize: 11,
                overflow: 'auto',
                maxHeight: 200,
                color: colors.TEXT_SECONDARY,
              }}>{ep.response}</pre>
            </div>
          )}

          {ep.auth !== 'sigv4' && (
            <Space>
              <Button
                type="primary"
                size="small"
                icon={<SendOutlined />}
                onClick={tryIt}
                loading={loading}
                style={{ borderRadius: 6 }}
              >
                Try it
              </Button>
              {ep.body && (
                <Button size="small" icon={<CopyOutlined />} onClick={() => navigator.clipboard.writeText(body)} style={{ borderRadius: 6 }}>
                  Copy body
                </Button>
              )}
            </Space>
          )}

          {ep.auth === 'sigv4' && (
            <Alert type="info" showIcon message="S3 API endpoints require SigV4 signing — use aws-cli or an S3 SDK." style={{ borderRadius: 6 }} />
          )}

          {response && (
            <div style={{ marginTop: 12 }}>
              <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', fontWeight: 600 }}>Live Response</Text>
              <pre style={{
                background: colors.BG_CARD,
                border: `1px solid ${colors.BORDER}`,
                borderRadius: 6,
                padding: 12,
                margin: '4px 0 0',
                fontFamily: 'var(--font-mono)',
                fontSize: 11,
                overflow: 'auto',
                maxHeight: 300,
                color: colors.TEXT_PRIMARY,
              }}>{response}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

interface ApiDocsPageProps {
  onBack: () => void;
}

export default function ApiDocsPage({ onBack }: ApiDocsPageProps) {
  const section = (title: string, description: string, endpoints: Endpoint[]) => (
    <div style={{ marginBottom: 32 }}>
      <Title level={5} style={{ fontFamily: 'var(--font-ui)', marginBottom: 4 }}>{title}</Title>
      <Text type="secondary" style={{ display: 'block', marginBottom: 16, fontSize: 13 }}>{description}</Text>
      {endpoints.map((ep, i) => <EndpointCard key={i} ep={ep} />)}
    </div>
  );

  return (
    <div style={{ maxWidth: 900, margin: '0 auto', padding: 'clamp(16px, 3vw, 24px)' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 24 }}>
        <div>
          <Title level={4} style={{ margin: 0, fontFamily: 'var(--font-ui)' }}>API Reference</Title>
          <Text type="secondary" style={{ fontSize: 13 }}>Interactive documentation — click any endpoint to expand and try it.</Text>
        </div>
        <Button onClick={onBack} icon={<ArrowLeftOutlined />} style={{ borderRadius: 8 }}>Back</Button>
      </div>

      {section('Operations', 'Health, metrics, and statistics — always public, no auth required.', OPS_API)}
      {section('Admin API', 'Proxy configuration and management. Requires admin session cookie.', ADMIN_API)}
      {section('IAM Users', 'Multi-user access control. Requires admin session cookie.', IAM_API)}
      {section('S3 API', 'S3-compatible object storage operations on port 9000. Requires SigV4 authentication.', S3_API)}
    </div>
  );
}
