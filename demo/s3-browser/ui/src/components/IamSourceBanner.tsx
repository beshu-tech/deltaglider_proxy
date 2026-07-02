/**
 * IamSourceBanner — one quiet line telling the operator where this
 * data lives (encrypted DB vs YAML), so "I added a user, why does
 * Copy YAML show access: {}?" never surprises anyone. The mode key
 * (access.iam_mode) rides as a hover chip, not baked into the prose.
 */
import { useColors } from '../ThemeContext';
import { DatabaseOutlined, FileTextOutlined } from '@ant-design/icons';
import type { IamMode } from '../adminApi';

interface Props {
  iamMode: IamMode | undefined;
  /** "users", "groups", "OAuth providers", or "mapping rules" — used in the copy. */
  resource: string;
}

export default function IamSourceBanner({ iamMode, resource }: Props) {
  const colors = useColors();
  const isDeclarative = iamMode === 'declarative';
  const accent = isDeclarative ? colors.ACCENT_AMBER : colors.ACCENT_BLUE;

  const text = isDeclarative
    ? `Read-only — your YAML config owns ${resource}. Edit it and apply to make changes.`
    : `${capitalise(resource)} live in the encrypted database, not YAML — use Full Backup to export everything.`;

  return (
    <div
      role="note"
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 8,
        padding: '8px 12px',
        marginBottom: 16,
        borderLeft: `3px solid ${accent}`,
        background: colors.BG_ELEVATED,
        borderRadius: 6,
        fontFamily: 'var(--font-ui)',
        fontSize: 12.5,
        color: colors.TEXT_SECONDARY,
        lineHeight: 1.5,
      }}
    >
      <span style={{ color: accent, fontSize: 14, flexShrink: 0 }} aria-hidden>
        {isDeclarative ? <FileTextOutlined /> : <DatabaseOutlined />}
      </span>
      <span>
        {text}{' '}
        <code
          title="YAML key controlling where IAM state lives"
          style={{ fontFamily: 'var(--font-mono)', fontSize: 11, opacity: 0.6, color: colors.TEXT_MUTED }}
        >
          access.iam_mode: {isDeclarative ? 'declarative' : 'gui'}
        </code>
      </span>
    </div>
  );
}

function capitalise(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}
