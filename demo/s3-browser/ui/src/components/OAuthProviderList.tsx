import { SafetyOutlined } from '@ant-design/icons';
import type { ExternalProviderInfo } from '../adminApi';
import { useColors } from '../ThemeContext';

interface Props {
  providers: ExternalProviderInfo[];
  /** The `next` param for the OAuth authorize redirect */
  nextUrl: string;
  /** Button height — use 48 for full-size (ConnectPage) or undefined for default */
  height?: number;
  /** Font size for button text */
  fontSize?: number;
}

/**
 * Renders a list of OAuth provider sign-in buttons.
 * Used by both ConnectPage and AdminPage login gate.
 */
export default function OAuthProviderList({ providers, nextUrl, height, fontSize = 14 }: Props) {
  const colors = useColors();

  if (providers.length === 0) return null;

  return (
    <div>
      {providers.map(p => (
        <a
          key={p.name}
          href={`/_/api/admin/oauth/authorize/${encodeURIComponent(p.name)}?next=${encodeURIComponent(nextUrl)}`}
          style={{
            display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 10,
            width: '100%', padding: '10px 16px', marginBottom: 8,
            borderRadius: 10, border: `1px solid ${colors.BORDER}`,
            background: 'var(--input-bg)', color: colors.TEXT_PRIMARY,
            fontSize, fontWeight: 600, fontFamily: 'var(--font-ui)',
            textDecoration: 'none', cursor: 'pointer',
            transition: 'border-color 0.15s, background 0.15s',
            ...(height ? { height } : {}),
          }}
          onMouseEnter={e => { e.currentTarget.style.borderColor = colors.ACCENT_BLUE; }}
          onMouseLeave={e => { e.currentTarget.style.borderColor = colors.BORDER; }}
        >
          <SafetyOutlined style={height ? { fontSize: 18 } : undefined} />
          Sign in with {p.display_name}
        </a>
      ))}
    </div>
  );
}
