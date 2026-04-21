/**
 * ShortcutsHelp — Wave 10 / 10.1, §10.3 of the admin UI revamp plan.
 *
 * Modal summarising the keyboard shortcuts the admin UI respects.
 * Triggered by `?` (when the focus is NOT inside an input /
 * textarea / editable element — we don't want a literal "?" in a
 * password field to open a help modal).
 *
 * Platform-aware (Wave 10.1): renders `⌘` on Apple and `Ctrl` on
 * everything else. A Mac user never sees the "Ctrl" duplicate;
 * a Windows user never sees the ⌘ glyph. Detection is a one-shot
 * at render time (see `platform.ts`). The keydown listener itself
 * in AdminPage still accepts BOTH modifiers — someone on a Mac
 * with a PC keyboard can still press Ctrl+K and everything works.
 */
import { Modal, Typography } from 'antd';
import { useColors } from '../ThemeContext';
import { metaKeyLabel } from '../platform';

const { Text } = Typography;

interface Shortcut {
  keys: string[];
  description: string;
}

/**
 * Build the shortcut list for the current platform. Pulling this
 * through `metaKeyLabel()` at render time means the list is
 * trivially correct on both Mac and Windows/Linux — no duplicate
 * "same as X on non-Apple" noise rows.
 */
function buildShortcuts(): Shortcut[] {
  const mod = metaKeyLabel(); // "⌘" on Apple, "Ctrl" elsewhere
  return [
    { keys: [mod, 'K'], description: 'Open command palette (quick nav)' },
    { keys: [mod, 'S'], description: 'Apply the current dirty section (if any)' },
    { keys: ['?'], description: 'Open this shortcuts reference' },
    { keys: ['Esc'], description: 'Close the palette / active modal' },
    { keys: ['↑', '↓'], description: 'Move cursor up/down in the command palette' },
    { keys: ['Enter'], description: 'Run the highlighted command' },
  ];
}

interface Props {
  open: boolean;
  onClose: () => void;
}

export default function ShortcutsHelp({ open, onClose }: Props) {
  const colors = useColors();
  // Computed at render time so we don't freeze the list at module
  // load — cheap, and keeps the detection logic owned by platform.ts.
  const shortcuts = buildShortcuts();
  return (
    <Modal
      open={open}
      onCancel={onClose}
      footer={null}
      title="Keyboard shortcuts"
      width={480}
      destroyOnHidden
    >
      <table
        style={{
          width: '100%',
          borderCollapse: 'collapse',
          fontSize: 13,
          fontFamily: 'var(--font-ui)',
        }}
      >
        <tbody>
          {shortcuts.map((s) => (
            <tr key={s.keys.join('+') + s.description} style={{ borderBottom: `1px solid ${colors.BORDER}` }}>
              <td style={{ padding: '10px 12px 10px 0', width: 160 }}>
                <KeyCombo keys={s.keys} />
              </td>
              <td style={{ padding: '10px 0', color: colors.TEXT_SECONDARY }}>
                {s.description}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 12, lineHeight: 1.6 }}>
        Tip: the palette accepts fuzzy input, e.g.{' '}
        <code style={{ fontFamily: 'var(--font-mono)' }}>adm cred</code>{' '}
        matches "Credentials &amp; mode" under Access.
      </Text>
    </Modal>
  );
}

function KeyCombo({ keys }: { keys: string[] }) {
  return (
    <span style={{ display: 'inline-flex', gap: 4, alignItems: 'center' }}>
      {keys.map((k, i) => (
        <span key={i} style={{ display: 'inline-flex', alignItems: 'center', gap: 4 }}>
          <Kbd>{k}</Kbd>
          {i < keys.length - 1 && <span style={{ fontSize: 10, color: '#888' }}>+</span>}
        </span>
      ))}
    </span>
  );
}

function Kbd({ children }: { children: React.ReactNode }) {
  const { BORDER, BG_ELEVATED, TEXT_PRIMARY } = useColors();
  return (
    <kbd
      style={{
        display: 'inline-block',
        minWidth: 20,
        padding: '2px 8px',
        border: `1px solid ${BORDER}`,
        background: BG_ELEVATED,
        color: TEXT_PRIMARY,
        borderRadius: 4,
        fontFamily: 'var(--font-mono)',
        fontSize: 11,
        lineHeight: 1.4,
        textAlign: 'center',
      }}
    >
      {children}
    </kbd>
  );
}
