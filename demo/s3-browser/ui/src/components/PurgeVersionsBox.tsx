import { useState } from 'react';
import { Checkbox, Typography } from 'antd';
import { PURGE_EXPLANATION, PURGE_LABEL } from '../conversionView';

const { Text } = Typography;

/**
 * The box of a conversion dialog for a versioned bucket: ask the re-encrypt
 * job to delete the old unencrypted versions. Unchecked by default. Without
 * `checked`, the box keeps its own state (a confirm dialog's static content)
 * and reports each change through `onChange`.
 */
export default function PurgeVersionsBox({
  checked,
  onChange,
}: {
  checked?: boolean;
  onChange: (purge: boolean) => void;
}) {
  const [own, setOwn] = useState(false);
  const value = checked ?? own;
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
      <Checkbox
        checked={value}
        data-testid="conversion-purge"
        onChange={(e) => {
          setOwn(e.target.checked);
          onChange(e.target.checked);
        }}
      >
        <span style={{ fontSize: 12 }}>{PURGE_LABEL}</span>
      </Checkbox>
      <Text type="secondary" style={{ fontSize: 12, paddingLeft: 24 }}>
        {PURGE_EXPLANATION}
      </Text>
    </div>
  );
}
