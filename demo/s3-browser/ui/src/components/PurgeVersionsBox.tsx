import { useId, useState } from 'react';
import { Checkbox, Typography } from 'antd';
import { PURGE_EXPLANATION, PURGE_LABEL } from '../conversionView';

const { Text } = Typography;

/**
 * The box of a conversion dialog for a versioned bucket: ask the re-encrypt
 * job to delete the old unencrypted versions. Unchecked by default, and
 * never the dialog's initial focus (a Space press must not tick a
 * permanent delete). Without `checked`, the box keeps its own state (a
 * confirm dialog's static content) and reports each change through
 * `onChange`. The label is the Checkbox's child (AntD's own gap); the help
 * is a plain secondary line under it, and the box's accessible description.
 */
export default function PurgeVersionsBox({
  checked,
  onChange,
}: {
  checked?: boolean;
  onChange: (purge: boolean) => void;
}) {
  const [own, setOwn] = useState(false);
  const helpId = useId();
  const value = checked ?? own;
  return (
    <div data-testid="conversion-purge">
      <Checkbox
        checked={value}
        aria-describedby={helpId}
        onChange={(e) => {
          setOwn(e.target.checked);
          onChange(e.target.checked);
        }}
      >
        <Text style={{ fontSize: 12 }}>{PURGE_LABEL}</Text>
      </Checkbox>
      <Text id={helpId} type="secondary" style={{ fontSize: 12, display: 'block' }}>
        {PURGE_EXPLANATION}
      </Text>
    </div>
  );
}
