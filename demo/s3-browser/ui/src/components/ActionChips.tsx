import {
  EyeOutlined,
  DownloadOutlined,
  UploadOutlined,
  DeleteOutlined,
  CrownOutlined,
  CheckOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { effectiveActions, toggleAction } from './permissionActions';

/**
 * The "CAN DO" row: the five atomic IAM actions as a tidy horizontal strip of
 * toggle-chips (multi-select, NOT a cumulative ladder — so "write without
 * delete" is expressible). Filled chip = granted; outlined = off.
 *
 * Model note: the action set is {read, write, delete, list, admin}. "Browse"
 * and "list" are the SAME action, so there is one List chip (not a separate
 * Browse) to avoid implying a distinction the engine doesn't make. `admin`
 * maps to bucket-level ops (Create/DeleteBucket), so the Admin chip is
 * disabled when the grant is scoped to a sub-prefix (where bucket ops are
 * meaningless) — it's only offered at bucket scope.
 *
 * The wildcard `*` action is represented as "all five filled"; toggling any
 * chip off from that state expands `*` into the explicit remaining set so the
 * UI never silently drops to a partial grant.
 */

interface ActionChipDef {
  value: string;
  label: string;
  /** One-line meaning shown as a native title tooltip. */
  hint: string;
  icon: React.ReactNode;
  /** Theme token key for the filled/accent colour. */
  tone: 'list' | 'read' | 'write' | 'delete' | 'admin';
}

const ACTION_CHIPS: ActionChipDef[] = [
  { value: 'list', label: 'List', hint: 'Browse / list this prefix (ListObjects)', icon: <EyeOutlined />, tone: 'list' },
  { value: 'read', label: 'Read', hint: 'Download objects (GET / HEAD)', icon: <DownloadOutlined />, tone: 'read' },
  { value: 'write', label: 'Write', hint: 'Upload / overwrite objects (PUT)', icon: <UploadOutlined />, tone: 'write' },
  { value: 'delete', label: 'Delete', hint: 'Delete objects (DELETE)', icon: <DeleteOutlined />, tone: 'delete' },
  { value: 'admin', label: 'Admin', hint: 'Bucket-level ops (Create / Delete bucket). Only meaningful at bucket scope.', icon: <CrownOutlined />, tone: 'admin' },
];

interface Props {
  actions: string[];
  onChange: (next: string[]) => void;
  /** True when the grant targets a sub-prefix (disables the Admin chip). */
  prefixScoped: boolean;
  disabled?: boolean;
}

export default function ActionChips({ actions, onChange, prefixScoped, disabled }: Props) {
  const colors = useColors();
  const held = effectiveActions(actions);

  const toneColor = (tone: ActionChipDef['tone']): string => {
    switch (tone) {
      case 'list': return colors.ACCENT_BLUE;     // teal
      case 'read': return colors.ACCENT_GREEN;
      case 'write': return colors.ACCENT_AMBER;
      case 'delete': return colors.ACCENT_RED;
      case 'admin': return colors.ACCENT_RED;
    }
  };

  return (
    <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6, alignItems: 'center' }}>
      {ACTION_CHIPS.map((chip) => {
        const on = held.has(chip.value);
        const adminBlocked = chip.value === 'admin' && prefixScoped;
        const isDisabled = disabled || adminBlocked;
        const accent = toneColor(chip.tone);
        return (
          <button
            key={chip.value}
            type="button"
            role="checkbox"
            aria-checked={on}
            aria-label={chip.label}
            disabled={isDisabled}
            title={adminBlocked ? `${chip.hint} — disabled: this grant is scoped to a prefix.` : `${chip.hint} — click to ${on ? 'remove' : 'add'}`}
            onClick={() => !isDisabled && onChange(toggleAction(actions, chip.value))}
            style={{
              display: 'inline-flex',
              alignItems: 'center',
              gap: 6,
              padding: '3px 10px 3px 7px',
              height: 28,
              borderRadius: 7,
              fontSize: 12,
              fontWeight: 600,
              cursor: isDisabled ? 'not-allowed' : 'pointer',
              userSelect: 'none',
              whiteSpace: 'nowrap',
              border: `1px ${on ? 'solid' : 'dashed'} ${on ? accent : colors.BORDER}`,
              background: on ? accent : 'transparent',
              color: on ? '#fff' : (isDisabled ? colors.TEXT_MUTED : colors.TEXT_SECONDARY),
              opacity: isDisabled && !on ? 0.45 : 1,
              transition: 'background 0.12s, border-color 0.12s, color 0.12s',
            }}
          >
            {/* Leading checkbox indicator makes the on/off toggle unmistakable. */}
            <span
              aria-hidden
              style={{
                width: 15,
                height: 15,
                borderRadius: 4,
                display: 'inline-flex',
                alignItems: 'center',
                justifyContent: 'center',
                flex: '0 0 auto',
                border: `1.5px solid ${on ? 'rgba(255,255,255,0.9)' : colors.BORDER}`,
                background: on ? 'rgba(255,255,255,0.18)' : 'transparent',
                fontSize: 10,
              }}
            >
              {on && <CheckOutlined style={{ fontSize: 9, color: '#fff' }} />}
            </span>
            <span aria-hidden style={{ fontSize: 13, display: 'inline-flex', opacity: on ? 1 : 0.85 }}>{chip.icon}</span>
            {chip.label}
          </button>
        );
      })}
    </div>
  );
}
