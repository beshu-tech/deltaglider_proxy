/**
 * IamSourceBanner — "where does this data live?" banner for the
 * IAM panels (Users, Groups, External authentication).
 *
 * ## Why
 *
 * The YAML config's `access:` section carries only five fields:
 * `iam_mode`, `authentication`, `access_key_id`,
 * `secret_access_key`, and `bootstrap_password_hash` (the last via
 * the advanced section actually). It does NOT carry users, groups,
 * OAuth providers, or mapping rules — those live in the encrypted
 * SQLCipher `deltaglider_config.db` file, by design, in GUI mode.
 *
 * Operators routinely hit this mismatch:
 *
 *   "I added a user. Why does Copy YAML show `access: {}`?"
 *
 * Because the user is in the DB; `access:` in YAML is the
 * credential-and-mode slice, not the identity directory. This
 * banner sits at the top of the IAM panels and explains the
 * architecture in scannable key/value shape.
 *
 * ## Visual design
 *
 * Not a generic AntD Alert. Horizontally arranged: an iconic
 * left column (the source-of-truth artefact), a headline, a
 * one-line summary, and a "Source ▸ Target" fact row. The most
 * important next-action (Full Backup export in GUI mode) renders
 * as a right-side action hint, not buried mid-prose.
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

  // Colour language: teal for the default, stable path (GUI);
  // amber for the locked-down, edit-YAML-only path (declarative).
  const accent = isDeclarative ? colors.ACCENT_AMBER : colors.ACCENT_BLUE;
  const accentBg = isDeclarative
    ? 'rgba(251, 191, 36, 0.06)'
    : 'rgba(45, 212, 191, 0.06)';
  const accentBorder = isDeclarative
    ? 'rgba(251, 191, 36, 0.25)'
    : 'rgba(45, 212, 191, 0.25)';

  const title = isDeclarative
    ? 'YAML is the source of truth'
    : `${capitalise(resource)} live in the encrypted database`;

  const summary = isDeclarative
    ? `Edits here are disabled. Change ${resource} by updating your YAML and applying the document.`
    : `YAML exports do not include ${resource}. Take a Full Backup when you need everything in one file.`;

  return (
    <div
      role="note"
      aria-label={title}
      style={{
        display: 'flex',
        gap: 16,
        alignItems: 'stretch',
        padding: '14px 18px',
        marginBottom: 20,
        background: accentBg,
        border: `1px solid ${accentBorder}`,
        borderLeft: `3px solid ${accent}`,
        borderRadius: 10,
        fontFamily: 'var(--font-ui)',
      }}
    >
      {/* Iconic column — shows which artefact is authoritative. */}
      <div
        style={{
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
          width: 40,
          color: accent,
          fontSize: 22,
          flexShrink: 0,
        }}
        aria-hidden
      >
        {isDeclarative ? <FileTextOutlined /> : <DatabaseOutlined />}
      </div>

      {/* Body */}
      <div style={{ flex: 1, minWidth: 0 }}>
        <div
          style={{
            fontSize: 14,
            fontWeight: 600,
            color: colors.TEXT_PRIMARY,
            letterSpacing: '-0.005em',
            marginBottom: 2,
          }}
        >
          {title}
        </div>
        <div
          style={{
            fontSize: 13,
            color: colors.TEXT_SECONDARY,
            lineHeight: 1.55,
            marginBottom: 10,
          }}
        >
          {summary}
        </div>

        {/* Fact row — scannable key/value pairs. */}
        <div
          style={{
            display: 'flex',
            flexWrap: 'wrap',
            columnGap: 24,
            rowGap: 6,
            fontSize: 12,
            color: colors.TEXT_MUTED,
          }}
        >
          <Fact label="Mode">
            <code style={{ fontFamily: 'var(--font-mono)', color: colors.TEXT_PRIMARY }}>
              {isDeclarative ? 'access.iam_mode: declarative' : 'access.iam_mode: gui'}
            </code>
          </Fact>
          <Fact label="Stored in">
            <span style={{ color: colors.TEXT_PRIMARY }}>
              {isDeclarative ? 'Your YAML config' : 'Encrypted IAM database'}
            </span>
          </Fact>
          <Fact label="Edit via">
            <span style={{ color: colors.TEXT_PRIMARY }}>
              {isDeclarative ? 'YAML apply' : 'This panel'}
            </span>
          </Fact>
        </div>
      </div>
    </div>
  );
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <span style={{ display: 'inline-flex', gap: 6, alignItems: 'baseline' }}>
      <span
        style={{
          fontSize: 10,
          fontWeight: 700,
          textTransform: 'uppercase',
          letterSpacing: '0.06em',
          opacity: 0.7,
        }}
      >
        {label}
      </span>
      {children}
    </span>
  );
}

function capitalise(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1);
}
