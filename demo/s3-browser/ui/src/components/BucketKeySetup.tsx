/**
 * BucketKeySetup — choose the proxy-AES key of one bucket (issue #116).
 *
 * Shared by the bucket card's Encryption group and the create-bucket
 * dialog. The operator either generates a key in the browser
 * (`generateAesKeyHex`, Web Crypto) or pastes one. A generated key is shown
 * once, with Copy, a key-file download (bucket, key id, date, recovery
 * note) and the YAML / environment snippet that keeps the key out of the
 * config file. Instead of an "I stored it" checkbox, the operator types the
 * last 6 characters of the generated key: that proves a copy exists
 * outside this page.
 *
 * The key id is explicit (the server requires it on a bucket): it is
 * stamped on every object, so it must never depend on a name. The
 * suggestion is `<bucket>-<yyyy>-<mm>`.
 */
import { useEffect, useId, useState } from 'react';
import { Alert, Button, Input, Radio, Space, Typography } from 'antd';
import { DownloadOutlined, SafetyOutlined, WarningOutlined } from '@ant-design/icons';
import { generateAesKeyHex } from '../aesKeyGen';
import { downloadText, useCopyToClipboard } from '../useCopyToClipboard';
import {
  bucketKeyYaml,
  isAesKeyHex,
  isValidKeyId,
  keyFileName,
  keyFileText,
  keyTailMatches,
  suggestKeyEnvVar,
  suggestKeyId,
} from '../bucketEncryption';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

export interface BucketKeyValue {
  key: string;
  keyId: string;
  /** The key and its id are valid, and a generated key is confirmed. */
  ready: boolean;
}

interface Props {
  bucket: string;
  /** The backend that stores the bucket (for the YAML snippet). */
  backend: string;
  /** Key ids the new id must differ from (the current and legacy ids). */
  usedKeyIds?: string[];
  onChange: (v: BucketKeyValue) => void;
}

export default function BucketKeySetup({ bucket, backend, usedKeyIds = [], onChange }: Props) {
  const uid = useId();
  const colors = useColors();
  const { copy } = useCopyToClipboard();
  const [source, setSource] = useState<'generate' | 'paste'>('generate');
  const [generated, setGenerated] = useState(() => generateAesKeyHex());
  const [pasted, setPasted] = useState('');
  const [keyId, setKeyId] = useState(() => suggestKeyId(bucket));
  const [tail, setTail] = useState('');
  // A renamed bucket (the create dialog) moves an untouched suggestion along.
  const [suggestedFor, setSuggestedFor] = useState(bucket);
  if (suggestedFor !== bucket) {
    setSuggestedFor(bucket);
    if (keyId === suggestKeyId(suggestedFor)) setKeyId(suggestKeyId(bucket));
  }

  const key = source === 'generate' ? generated : pasted.trim();
  const keyOk = isAesKeyHex(key);
  const idTaken = usedKeyIds.includes(keyId);
  const idOk = isValidKeyId(keyId) && !idTaken;
  // The confirmation covers the key, its id and the bucket together: a
  // change of any of them clears it (review P2, defect 3: an id edited
  // after the confirmation left a key file with another id).
  const binding = `${bucket}\u0000${keyId}\u0000${key}`;
  const [tailFor, setTailFor] = useState(binding);
  if (tailFor !== binding) {
    setTailFor(binding);
    if (tail) setTail('');
  }
  // A key file downloaded before such a change names the wrong bucket or id.
  const [downloadedFor, setDownloadedFor] = useState<string | null>(null);
  const staleFile = downloadedFor !== null && downloadedFor !== binding;
  const confirmed = source === 'paste' || keyTailMatches(key, tail);
  const ready = keyOk && idOk && confirmed && !staleFile;

  useEffect(() => {
    onChange({ key, keyId, ready });
    // `onChange` is the parent's setter; the value is what matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, keyId, ready]);

  const envVar = suggestKeyEnvVar(bucket);
  const yaml = bucketKeyYaml({ bucket, backend, keyId, envVar });
  const label = { fontSize: 11, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)', display: 'block', marginTop: 10 } as const;

  return (
    <div data-testid="bucket-key-setup">
      <Alert
        type="error"
        showIcon
        icon={<WarningOutlined />}
        style={{ borderRadius: 6, fontSize: 12 }}
        title="If you lose this key, the objects that it encrypts are unrecoverable."
        description={
          <span>
            Store the key in a password manager or a secrets vault before you apply. The full backup
            (the Backup card on the System page) includes the key while it is in the config file,
            but not a key that an environment variable supplies, and a backup on the same host is
            lost with the host.
          </span>
        }
      />
      <Radio.Group
        aria-label="Key source"
        value={source}
        onChange={(e) => {
          setSource(e.target.value);
          setTail('');
        }}
        size="small"
        style={{ marginTop: 10 }}
      >
        <Radio.Button value="generate">Generate a key</Radio.Button>
        <Radio.Button value="paste">Paste a key</Radio.Button>
      </Radio.Group>

      <label htmlFor={`${uid}-kid`} style={label}>
        Key id (stamped on every object; letters, digits, <code>.</code> <code>_</code> <code>-</code>)
      </label>
      <Input
        id={`${uid}-kid`}
        aria-label="Key id"
        size="small"
        value={keyId}
        onChange={(e) => setKeyId(e.target.value.trim())}
        status={idOk ? undefined : 'error'}
        style={{ fontFamily: 'var(--font-mono)', fontSize: 12, maxWidth: 320 }}
      />
      {!idOk && (
        <Text type="danger" role="alert" style={{ fontSize: 11, display: 'block' }}>
          {idTaken
            ? `The bucket already uses key id ${keyId}. A new key needs a new id.`
            : 'Use 1 to 64 letters, digits, dots, underscores or hyphens.'}
        </Text>
      )}

      {source === 'generate' ? (
        <>
          <label htmlFor={`${uid}-key`} style={label}>
            Generated key (64 hexadecimal characters, shown once)
          </label>
          <Input.TextArea
            id={`${uid}-key`}
            aria-label="Generated key"
            value={generated}
            readOnly
            rows={2}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 11, letterSpacing: 0.5 }}
          />
          <Space wrap style={{ marginTop: 6 }}>
            <Button size="small" onClick={() => void copy(generated, { successMessage: 'Key copied to clipboard' })}>
              Copy
            </Button>
            <Button
              size="small"
              icon={<DownloadOutlined />}
              onClick={() => {
                downloadText(keyFileText({ bucket, backend, keyId, key: generated }), keyFileName(bucket, keyId));
                setDownloadedFor(binding);
              }}
            >
              Download key file
            </Button>
            <Button
              size="small"
              icon={<SafetyOutlined />}
              onClick={() => {
                setGenerated(generateAesKeyHex());
                setTail('');
              }}
            >
              Generate another
            </Button>
          </Space>
          {staleFile && (
            <Text type="danger" role="alert" style={{ fontSize: 11, display: 'block', marginTop: 6 }}>
              The key file that you downloaded names another bucket or key id. Download it again.
            </Text>
          )}
          <label htmlFor={`${uid}-tail`} style={label}>
            To confirm that you stored the key, type its last 6 characters
          </label>
          <Input
            id={`${uid}-tail`}
            aria-label="Last 6 characters of the key"
            size="small"
            value={tail}
            maxLength={6}
            onChange={(e) => setTail(e.target.value)}
            status={tail.length === 6 && !confirmed ? 'error' : undefined}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 12, width: 120 }}
          />
          {tail.length === 6 && !confirmed && (
            <Text type="danger" role="alert" style={{ fontSize: 11, display: 'block' }}>
              These are not the last 6 characters of the key.
            </Text>
          )}
        </>
      ) : (
        <>
          <label htmlFor={`${uid}-paste`} style={label}>
            Key (64 hexadecimal characters)
          </label>
          <Input.Password
            id={`${uid}-paste`}
            aria-label="Pasted key"
            size="small"
            value={pasted}
            onChange={(e) => setPasted(e.target.value)}
            status={pasted && !keyOk ? 'error' : undefined}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}
          />
          {pasted && !keyOk && (
            <Text type="danger" role="alert" style={{ fontSize: 11, display: 'block' }}>
              A key is exactly 64 hexadecimal characters (0-9, a-f).
            </Text>
          )}
        </>
      )}

      <Text type="secondary" style={{ fontSize: 12, display: 'block', marginTop: 10 }}>
        When you apply here, the proxy writes the key into the config file. To keep it out of the
        file, set it in an environment variable and reference it from the YAML instead:
      </Text>
      <pre
        data-testid="bucket-key-yaml"
        style={{ fontSize: 11, background: colors.BG_CARD, padding: 8, borderRadius: 6, margin: '6px 0 0', overflowX: 'auto' }}
      >
        {`${envVar}=<the key>\n\n${yaml}`}
      </pre>
    </div>
  );
}
