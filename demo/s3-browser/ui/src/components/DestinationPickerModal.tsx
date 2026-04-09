import { useState, useEffect } from 'react';
import { Modal, Input, Alert, Typography } from 'antd';
import { WarningOutlined } from '@ant-design/icons';
import { listBuckets, getBucket } from '../s3client';
import { useColors } from '../ThemeContext';
import SimpleSelect from './SimpleSelect';

const { Text } = Typography;

interface Props {
  open: boolean;
  mode: 'copy' | 'move';
  itemCount: number;
  onConfirm: (destBucket: string, destPrefix: string) => void;
  onCancel: () => void;
  loading: boolean;
}

export default function DestinationPickerModal({ open, mode, itemCount, onConfirm, onCancel, loading }: Props) {
  const colors = useColors();
  const [buckets, setBuckets] = useState<string[]>([]);
  const [destBucket, setDestBucket] = useState(getBucket());
  const [destPrefix, setDestPrefix] = useState('');

  useEffect(() => {
    if (open) {
      listBuckets().then(bs => setBuckets(bs.map(b => b.name))).catch(() => {});
      setDestBucket(getBucket());
      setDestPrefix('');
    }
  }, [open]);

  const clean = destPrefix.replace(/^\/+/, '').replace(/\/+$/, '');
  const preview = `${destBucket}/${clean ? clean + '/' : ''}`;

  return (
    <Modal
      open={open}
      title={`${mode === 'move' ? 'Move' : 'Copy'} ${itemCount} item${itemCount !== 1 ? 's' : ''}`}
      onCancel={onCancel}
      onOk={() => onConfirm(destBucket, clean ? clean + '/' : '')}
      okText={`${mode === 'move' ? 'Move' : 'Copy'} ${itemCount} item${itemCount !== 1 ? 's' : ''}`}
      okButtonProps={{ loading, disabled: !destBucket }}
      cancelButtonProps={{ disabled: loading }}
      destroyOnClose
      maskClosable={!loading}
    >
      <div style={{ marginBottom: 16 }}>
        <Text style={{ fontSize: 12, fontWeight: 600, color: colors.TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 0.5, display: 'block', marginBottom: 6 }}>
          Destination Bucket
        </Text>
        <SimpleSelect
          value={destBucket}
          onChange={setDestBucket}
          options={buckets.map(b => ({ value: b, label: b }))}
          placeholder="Select bucket"
          style={{ width: '100%' }}
        />
      </div>

      <div style={{ marginBottom: 16 }}>
        <Text style={{ fontSize: 12, fontWeight: 600, color: colors.TEXT_MUTED, textTransform: 'uppercase', letterSpacing: 0.5, display: 'block', marginBottom: 6 }}>
          Destination Path
        </Text>
        <Input
          value={destPrefix}
          onChange={e => setDestPrefix(e.target.value)}
          placeholder="/ (bucket root)"
          style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}
          autoFocus
        />
      </div>

      <div style={{
        padding: '8px 12px', borderRadius: 6,
        background: colors.BG_BASE, border: `1px solid ${colors.BORDER}`,
        marginBottom: mode === 'move' ? 12 : 0,
      }}>
        <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>Preview: </Text>
        <Text style={{ fontSize: 12, fontFamily: 'var(--font-mono)', color: colors.ACCENT_BLUE }}>{preview}</Text>
      </div>

      {mode === 'move' && (
        <Alert
          type="warning"
          icon={<WarningOutlined />}
          message="Source files will be deleted after successful copy."
          showIcon
          style={{ borderRadius: 8 }}
        />
      )}
    </Modal>
  );
}
