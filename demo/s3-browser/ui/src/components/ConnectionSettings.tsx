import { useState } from 'react';
import { Popover, Form, Input, Button, Badge, Space } from 'antd';
import { ApiOutlined } from '@ant-design/icons';
import { getEndpoint, setEndpoint, getCredentials, setCredentials } from '../s3client';

interface Props {
  connected: boolean;
  isMobile?: boolean;
}

export default function ConnectionSettings({ connected, isMobile }: Props) {
  const [open, setOpen] = useState(false);
  const [ep, setEp] = useState(getEndpoint());
  const [accessKey, setAccessKey] = useState(getCredentials().accessKeyId);
  const [secretKey, setSecretKey] = useState(getCredentials().secretAccessKey);

  const handleEndpoint = (val: string) => {
    setEp(val);
    setEndpoint(val);
  };

  const handleAccessKey = (val: string) => {
    setAccessKey(val);
    setCredentials(val, secretKey);
  };

  const handleSecretKey = (val: string) => {
    setSecretKey(val);
    setCredentials(accessKey, val);
  };

  const content = (
    <Form layout="vertical" style={{ width: isMobile ? 240 : 280 }}>
      <Form.Item label="Proxy Endpoint" style={{ marginBottom: 12 }}>
        <Input
          value={ep}
          onChange={(e) => handleEndpoint(e.target.value)}
          placeholder="http://localhost:9002"
        />
      </Form.Item>
      <Form.Item label="Access Key ID" style={{ marginBottom: 12 }}>
        <Input
          value={accessKey}
          onChange={(e) => handleAccessKey(e.target.value)}
          placeholder="access key id"
        />
      </Form.Item>
      <Form.Item label="Secret Access Key" style={{ marginBottom: 0 }}>
        <Input.Password
          value={secretKey}
          onChange={(e) => handleSecretKey(e.target.value)}
          placeholder="secret access key"
        />
      </Form.Item>
    </Form>
  );

  return (
    <Popover
      content={content}
      title="Connection Settings"
      trigger="click"
      open={open}
      onOpenChange={setOpen}
      placement="bottomRight"
    >
      <Button type="text">
        <Space size={6}>
          <Badge status={connected ? 'success' : 'error'} />
          <ApiOutlined />
          {!isMobile && <span>Connection</span>}
        </Space>
      </Button>
    </Popover>
  );
}
