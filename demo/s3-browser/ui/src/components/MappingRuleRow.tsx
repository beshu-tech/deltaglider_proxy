import { Button, Typography, Input, Select } from 'antd';
import { DeleteOutlined } from '@ant-design/icons';
import type { AuthProvider, MappingRule, IamGroup } from '../adminApi';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

interface MappingRuleRowProps {
  rule: MappingRule;
  providers: AuthProvider[];
  groups: IamGroup[];
  colors: ReturnType<typeof useColors>;
  onUpdate: (req: Record<string, unknown>) => void;
  /** Confirms and handles its own errors; the row only fires it. */
  onDelete: () => void | Promise<void>;
  /** Locks all inputs while a Save Rules round-trip is in flight, so a
   *  concurrent edit can't be lost when loadData() resyncs afterwards. */
  disabled?: boolean;
}

const MATCH_TYPES = [
  { value: 'email_glob', label: 'Email pattern' },
  { value: 'email_domain', label: 'Email domain' },
  { value: 'email_exact', label: 'Email exact' },
  { value: 'email_regex', label: 'Email regex' },
  { value: 'claim_value', label: 'Claim value' },
];

export default function MappingRuleRow({ rule, providers, groups, colors, onUpdate, onDelete, disabled }: MappingRuleRowProps) {
  return (
    <div style={{
      display: 'flex', alignItems: 'center', gap: 8, padding: '8px 12px',
      background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 6,
      flexWrap: 'wrap',
    }}>
      <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, whiteSpace: 'nowrap' }}>When</Text>
      <Select
        size="small"
        disabled={disabled}
        value={rule.match_type}
        onChange={v => onUpdate({ match_type: v })}
        options={MATCH_TYPES.map(t => ({ value: t.value, label: t.label }))}
        style={{ width: 140 }}
      />
      {rule.match_type === 'claim_value' && (
        <>
          <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>field</Text>
          <Input
            size="small"
            disabled={disabled}
            value={rule.match_field}
            onChange={e => onUpdate({ match_field: e.target.value })}
            style={{ width: 80 }}
          />
        </>
      )}
      <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>matches</Text>
      <Input
        size="small"
        disabled={disabled}
        value={rule.match_value}
        onChange={e => onUpdate({ match_value: e.target.value })}
        style={{ width: 180 }}
        placeholder={
          rule.match_type === 'email_glob' ? '*@company.com' :
          rule.match_type === 'email_domain' ? 'company.com' :
          rule.match_type === 'email_exact' ? 'alice@company.com' :
          'value'
        }
      />
      <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, whiteSpace: 'nowrap' }}>assign to</Text>
      <Select
        size="small"
        showSearch={{ optionFilterProp: 'label' }}
        disabled={disabled}
        value={String(rule.group_id)}
        onChange={v => onUpdate({ group_id: Number(v) })}
        options={groups.map(g => ({ value: String(g.id), label: g.name }))}
        style={{ width: 140 }}
      />
      <Select
        size="small"
        showSearch={{ optionFilterProp: 'label' }}
        disabled={disabled}
        value={String(rule.provider_id ?? 0)}
        onChange={v => onUpdate({ provider_id: Number(v) === 0 ? null : Number(v) })}
        options={[
          { value: '0', label: 'All providers' },
          ...providers.map(p => ({ value: String(p.id), label: p.display_name || p.name })),
        ]}
        style={{ width: 130 }}
      />
      <Button size="small" danger disabled={disabled} icon={<DeleteOutlined />} onClick={() => { void onDelete(); }} title="Delete rule" aria-label="Delete rule" />
    </div>
  );
}
