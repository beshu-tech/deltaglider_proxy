import { useState, useEffect } from 'react';
import { Modal, Input, Alert, Typography, Select } from 'antd';
import { listBuckets, getBucket } from '../s3client';
import { useColors } from '../ThemeContext';
import { pluralize } from '../utils';
import { SPACE } from './shared-styles';
import { normalizeDestPrefix, destinationIsSource, keyPathError, initialDestPrefix } from './destPrefix';

const { Text } = Typography;

interface Props {
  open: boolean;
  mode: 'copy' | 'move';
  itemCount: number;
  onConfirm: (destBucket: string, destPrefix: string) => void;
  onCancel: () => void;
  loading: boolean;
  /** The folder the user is browsing; the destination path starts there. */
  currentPrefix?: string;
  /** Selection keys (`folder:<prefix>` for folders) — used to refuse a copy onto itself. */
  selectionKeys?: Iterable<string>;
}

/** "Move 3 objects" / "Copy 1 object" — shared by the modal title and OK button. */
function getModalTitle(mode: 'copy' | 'move', itemCount: number): string {
  return `${mode === 'move' ? 'Move' : 'Copy'} ${pluralize(itemCount, 'object')}`;
}

/** Field label shared by the bucket/path inputs (sentence case, D67). */
function SectionLabel({ color, children, id }: { color: string; children: React.ReactNode; id?: string }) {
  return (
    <Text id={id} style={{ fontSize: 13, fontWeight: 600, color, display: 'block', marginBottom: SPACE.xxs, fontFamily: 'var(--font-ui)' }}>
      {children}
    </Text>
  );
}

export default function DestinationPickerModal({ open, mode, itemCount, onConfirm, onCancel, loading, currentPrefix = '', selectionKeys = [] }: Props) {
  const colors = useColors();
  const [buckets, setBuckets] = useState<string[]>([]);
  const [sourceBucket, setSourceBucket] = useState(getBucket());
  const [destBucket, setDestBucket] = useState(getBucket());
  const [destPrefix, setDestPrefix] = useState('');
  // Until the user changes the destination, a destination that is the
  // source gets a prompt, not the "already in this folder" alert.
  const [edited, setEdited] = useState(false);

  // The keys at the moment the picker opens: the effect must not reset the
  // path when the selection object changes identity.
  const keysText = [...selectionKeys].join('\n');
  useEffect(() => {
    if (open) {
      listBuckets().then(bs => setBuckets(bs.filter(b => !b.unavailable).map(b => b.name))).catch(() => {});
      const bucket = getBucket();
      setSourceBucket(bucket);
      setDestBucket(bucket);
      setEdited(false);
      // Start from the folder being browsed (copies usually go next to, or
      // below, where the user already is), or its parent when the
      // selection lives there: the picker never opens blocked.
      setDestPrefix(initialDestPrefix(bucket, keysText ? keysText.split('\n') : [], currentPrefix));
    }
  }, [open, currentPrefix, keysText]);

  const clean = normalizeDestPrefix(destPrefix);
  const preview = `${destBucket}/${clean ? clean + '/' : ''}`;
  // The path starts at the browsed folder, which IS where the selection lives:
  // a move there does nothing and a copy rewrites every object in place.
  const sameLocation = destinationIsSource(sourceBucket, selectionKeys, destBucket, destPrefix);
  const pathError = keyPathError(clean);

  return (
    <Modal
      open={open}
      title={getModalTitle(mode, itemCount)}
      onCancel={onCancel}
      onOk={() => onConfirm(destBucket, clean ? clean + '/' : '')}
      okText={getModalTitle(mode, itemCount)}
      okButtonProps={{ loading, disabled: !destBucket || sameLocation || pathError !== null }}
      cancelButtonProps={{ disabled: loading }}
      destroyOnHidden
      mask={{ closable: !loading }}
    >
      <div style={{ marginBottom: SPACE.md }}>
        <SectionLabel id="dest-picker-bucket" color={colors.TEXT_PRIMARY}>Destination bucket</SectionLabel>
        <Select
          aria-labelledby="dest-picker-bucket"
          value={destBucket}
          onChange={(b) => { setDestBucket(b); setEdited(true); }}
          options={buckets.map(b => ({ value: b, label: b }))}
          placeholder="Select bucket"
          style={{ width: '100%' }}
          showSearch={{ optionFilterProp: 'label' }}
        />
      </div>

      <div style={{ marginBottom: SPACE.md }}>
        <SectionLabel id="dest-picker-path" color={colors.TEXT_PRIMARY}>Destination folder</SectionLabel>
        <Input
          aria-labelledby="dest-picker-path"
          value={destPrefix}
          onChange={e => { setDestPrefix(e.target.value); setEdited(true); }}
          placeholder="/ (bucket root)"
          style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}
          autoFocus
          onFocus={(e) => e.currentTarget.select()}
          status={pathError ? 'error' : undefined}
          aria-invalid={pathError !== null}
        />
        {pathError && (
          <Text role="alert" style={{ display: 'block', marginTop: SPACE.xxs, fontSize: 12, color: colors.ACCENT_RED }}>
            {pathError}
          </Text>
        )}
      </div>

      <div style={{
        padding: `${SPACE.xs}px ${SPACE.sm}px`, borderRadius: SPACE.xs,
        background: colors.BG_BASE, border: `1px solid ${colors.BORDER}`,
        marginBottom: mode === 'move' || sameLocation ? SPACE.sm : 0,
      }}>
        <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>Preview: </Text>
        <Text style={{ fontSize: 12, fontFamily: 'var(--font-mono)', color: colors.ACCENT_BLUE }}>{preview}</Text>
      </div>

      {sameLocation && (
        <Alert
          type="info"
          showIcon
          title={edited
            ? `The selected objects are already in this folder. Choose another folder or bucket to ${mode} them.`
            : `Type the folder or choose the bucket to ${mode} the selected objects to.`}
          style={{ borderRadius: SPACE.xs, marginBottom: mode === 'move' ? SPACE.sm : 0 }}
        />
      )}

      {/* A fact of every move, not a state the user created: help text,
          not a warning box (UX harvest D52). */}
      {mode === 'move' && (
        <Text style={{ display: 'block', fontSize: 12, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
          A move copies each object, then deletes the source object once its copy succeeded.
        </Text>
      )}
    </Modal>
  );
}
