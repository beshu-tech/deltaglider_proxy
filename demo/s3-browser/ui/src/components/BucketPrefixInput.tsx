import { useEffect, useMemo, useState } from 'react';
import { Button, Typography } from 'antd';
import { listCommonPrefixes } from '../s3client';
import { normalizePrefix } from '../storagePath';
import { useColors } from '../ThemeContext';
import SimpleAutoComplete from './SimpleAutoComplete';

const { Text } = Typography;

interface BucketPrefixValue {
  bucket: string;
  prefix: string;
}

interface BucketPrefixInputProps {
  value: BucketPrefixValue;
  onChange: (value: BucketPrefixValue) => void;
  buckets?: string[];
  bucketPlaceholder?: string;
  prefixPlaceholder?: string;
  bucketLabel?: string;
  prefixLabel?: string;
  showHelp?: boolean;
  style?: React.CSSProperties;
}

export default function BucketPrefixInput({
  value,
  onChange,
  buckets = [],
  bucketPlaceholder = 'prod-artifacts',
  prefixPlaceholder = 'releases/',
  bucketLabel = 'Bucket',
  prefixLabel = 'Prefix',
  showHelp = true,
  style,
}: BucketPrefixInputProps) {
  const colors = useColors();
  const [prefixOptions, setPrefixOptions] = useState<string[]>([]);
  const normalized = useMemo(() => normalizePrefix(value.prefix), [value.prefix]);
  const pathExamples = [
    { bucket: 'my-bucket', prefix: '', label: 'my-bucket/*', desc: 'whole bucket' },
    { bucket: 'my-bucket', prefix: 'builds/', label: 'my-bucket/builds/*', desc: 'one prefix' },
    { bucket: 'archive-bucket', prefix: 'releases/', label: 'archive-bucket/releases/*', desc: 'archive prefix' },
  ];

  useEffect(() => {
    let cancelled = false;
    const bucket = value.bucket.trim();
    if (!bucket) {
      setPrefixOptions([]);
      return;
    }

    const timer = window.setTimeout(() => {
      listCommonPrefixes(bucket, normalizePrefix(value.prefix))
        .then((prefixes) => {
          if (!cancelled) setPrefixOptions(prefixes);
        })
        .catch(() => {
          if (!cancelled) setPrefixOptions([]);
        });
    }, 200);

    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [value.bucket, value.prefix]);

  return (
    <div style={{ ...style }}>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 10 }}>
      <div>
        <Text type="secondary" style={{ display: 'block', fontSize: 11, fontWeight: 600, marginBottom: 4 }}>
          {bucketLabel}
        </Text>
        <SimpleAutoComplete
          value={value.bucket}
          onChange={(bucket) => onChange({ ...value, bucket })}
          options={buckets}
          placeholder={bucketPlaceholder}
          inputTitle="Bucket name. Type a custom bucket or pick one of the discovered buckets."
          style={{ width: '100%' }}
        />
        <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
          {buckets.length > 0 ? `${buckets.length} discovered bucket${buckets.length === 1 ? '' : 's'} available.` : 'Type a bucket name; suggestions appear when the proxy can list buckets.'}
        </Text>
      </div>
      <div>
        <Text type="secondary" style={{ display: 'block', fontSize: 11, fontWeight: 600, marginBottom: 4 }}>
          {prefixLabel}
        </Text>
        <SimpleAutoComplete
          value={value.prefix}
          onChange={(prefix) => onChange({ ...value, prefix })}
          onBlur={() => onChange({ ...value, prefix: normalized })}
          options={prefixOptions}
          placeholder={prefixPlaceholder}
          inputTitle="Optional prefix inside the bucket. Leave empty for the whole bucket; slashes are normalized on blur."
          style={{ width: '100%' }}
        />
        {showHelp && (
          <div style={{ fontSize: 11, marginTop: 4, color: colors.TEXT_MUTED, lineHeight: 1.45 }}>
            {normalized ? <>Canonical prefix: <Text code>{normalized}</Text></> : 'Leave empty for the whole bucket.'}
            {' '}
            {value.bucket.trim()
              ? `${prefixOptions.length} prefix suggestion${prefixOptions.length === 1 ? '' : 's'} found.`
              : 'Choose a bucket to browse prefix suggestions.'}
          </div>
        )}
      </div>
      </div>
      {showHelp && (
        <details style={{ marginTop: 8 }}>
          <summary
            title="Open bucket and prefix examples"
            style={{
              cursor: 'pointer',
              color: colors.TEXT_SECONDARY,
              fontSize: 11,
              fontWeight: 600,
              userSelect: 'none',
            }}
          >
            Examples and path rules
          </summary>
          <div style={{ marginTop: 8, display: 'flex', flexWrap: 'wrap', gap: '6px 8px', alignItems: 'center' }}>
            {pathExamples.map((example) => (
              <Button
                key={example.label}
                type="text"
                size="small"
                onClick={() => onChange({ bucket: example.bucket, prefix: example.prefix })}
                title={`Use ${example.label}: ${example.desc}`}
                style={{
                  height: 24,
                  padding: '0 7px',
                  border: `1px solid ${colors.BORDER}`,
                  borderRadius: 5,
                  background: 'var(--input-bg)',
                  color: colors.ACCENT_BLUE,
                  fontFamily: 'var(--font-mono)',
                  fontSize: 11,
                }}
              >
                {example.label}
              </Button>
            ))}
          </div>
          <div style={{ marginTop: 6, color: colors.TEXT_MUTED, fontSize: 11, lineHeight: 1.5 }}>
            Prefixes are literal paths. The UI normalizes duplicate slashes and stores prefixes with a trailing slash; it does not expand variables.
          </div>
        </details>
      )}
    </div>
  );
}
