/**
 * ShortcutsHelp — app-wide keyboard-shortcuts reference modal.
 *
 * Lists every shortcut the app respects, grouped by scope (Global, Object
 * browser, Settings command palette). Triggered app-wide by `?` (when focus is
 * NOT in an input / textarea / editable element — we don't want a literal "?"
 * in a password field to open it) and by the help icon in the header.
 *
 * Platform-aware: renders `⌘` on Apple and `Ctrl` on everything else. A Mac
 * user never sees the "Ctrl" duplicate; a Windows user never sees the ⌘ glyph.
 * Detection is a one-shot at render time (see `platform.ts`). The keydown
 * listeners themselves accept BOTH modifiers — someone on a Mac with a PC
 * keyboard can still press Ctrl+K and everything works.
 */
import { Modal, Typography } from 'antd';
import { useColors } from '../ThemeContext';
import { metaKeyLabel } from '../platform';

const { Text } = Typography;

interface Shortcut {
  keys: string[];
  description: string;
}

interface ShortcutGroup {
  title: string;
  shortcuts: Shortcut[];
}

/**
 * Build the grouped shortcut list for the current platform. Pulling this
 * through `metaKeyLabel()` at render time means it's trivially correct on both
 * Mac and Windows/Linux — no duplicate "same as X on non-Apple" noise rows.
 */
function buildGroups(): ShortcutGroup[] {
  const mod = metaKeyLabel(); // "⌘" on Apple, "Ctrl" elsewhere
  return [
    {
      title: 'Global',
      shortcuts: [
        { keys: [mod, ','], description: 'Open Settings' },
        { keys: [mod, '/'], description: 'Open Docs' },
        { keys: ['?'], description: 'Open this shortcuts reference' },
      ],
    },
    {
      title: 'Object browser',
      shortcuts: [
        { keys: ['↑', '↓'], description: 'Move between objects and folders' },
        { keys: ['Enter'], description: 'Open folder / inspect object' },
        { keys: ['→'], description: 'Open folder / inspect object' },
        { keys: ['←'], description: 'Go up one folder' },
        { keys: ['Backspace'], description: 'Go up one folder' },
        { keys: ['Home', 'End'], description: 'Jump to first / last row' },
        { keys: ['Esc'], description: 'Close inspector, or go up one folder' },
      ],
    },
    {
      title: 'Settings — command palette',
      shortcuts: [
        { keys: [mod, 'K'], description: 'Open command palette (quick nav)' },
        { keys: [mod, 'S'], description: 'Apply the current dirty section (if any)' },
        { keys: ['↑', '↓'], description: 'Move cursor in the palette' },
        { keys: ['Enter'], description: 'Run the highlighted command' },
        { keys: ['Esc'], description: 'Close the palette / active modal' },
      ],
    },
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
  const groups = buildGroups();
  return (
    <Modal
      open={open}
      onCancel={onClose}
      footer={null}
      title="Keyboard shortcuts"
      width={480}
      destroyOnHidden
    >
      {groups.map((group) => (
        <div key={group.title} style={{ marginBottom: 18 }}>
          <Text
            style={{
              display: 'block',
              fontSize: 11,
              fontWeight: 600,
              letterSpacing: '0.04em',
              textTransform: 'uppercase',
              color: colors.TEXT_MUTED,
              marginBottom: 4,
            }}
          >
            {group.title}
          </Text>
          <table
            style={{
              width: '100%',
              borderCollapse: 'collapse',
              fontSize: 13,
              fontFamily: 'var(--font-ui)',
            }}
          >
            <tbody>
              {group.shortcuts.map((s) => (
                <tr
                  key={s.keys.join('+') + s.description}
                  style={{ borderBottom: `1px solid ${colors.BORDER}` }}
                >
                  <td style={{ padding: '8px 12px 8px 0', width: 160 }}>
                    <KeyCombo keys={s.keys} />
                  </td>
                  <td style={{ padding: '8px 0', color: colors.TEXT_SECONDARY }}>
                    {s.description}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ))}
      <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 4, lineHeight: 1.6 }}>
        Tip: the palette accepts fuzzy input, e.g.{' '}
        <code style={{ fontFamily: 'var(--font-mono)' }}>adm cred</code>{' '}
        matches "Credentials &amp; mode" under Access.
      </Text>
    </Modal>
  );
}

function KeyCombo({ keys }: { keys: string[] }) {
  const { TEXT_FAINT } = useColors();
  return (
    <span style={{ display: 'inline-flex', gap: 4, alignItems: 'center' }}>
      {keys.map((k, i) => (
        <span key={i} style={{ display: 'inline-flex', alignItems: 'center', gap: 4 }}>
          <Kbd>{k}</Kbd>
          {i < keys.length - 1 && <span style={{ fontSize: 10, color: TEXT_FAINT }}>+</span>}
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
