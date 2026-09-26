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
  /** Set when the config (and so the mode) failed to load: editing stays off. */
  loadError?: string;
  /** "users", "groups", "OAuth providers", or "mapping rules" — used in the copy. */
  resource: string;
}

export default function IamSourceBanner({ iamMode, loadError, resource }: Props) {
  const colors = useColors();
  // Mode unknown and still loading: say nothing rather than guess.
  if (!iamMode && !loadError) return null;
  const isDeclarative = iamMode === 'declarative';
  const accent = loadError ? colors.ACCENT_RED : isDeclarative ? colors.ACCENT_AMBER : colors.ACCENT_BLUE;

  const text = loadError
    ? `Read-only — could not load the IAM mode, so editing ${resource} is off until it loads. ${loadError}`
    : isDeclarative
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
        {!loadError && (
          <code
            title="YAML key controlling where IAM state lives"
            style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: colors.TEXT_MUTED }}
          >
            access.iam_mode: {isDeclarative ? 'declarative' : 'gui'}
          </code>
        )}
      </span>
    </div>
  );
}

function capitalise(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}
