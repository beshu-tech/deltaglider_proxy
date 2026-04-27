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
  /** Visual treatment for high-prominence login surfaces */
  variant?: 'default' | 'hero';
}

/**
 * Renders a list of OAuth provider sign-in buttons.
 * Used by both ConnectPage and AdminPage login gate.
 */
export default function OAuthProviderList({ providers, nextUrl, height, fontSize = 14, variant = 'default' }: Props) {
  const colors = useColors();
  const isHero = variant === 'hero';

  if (providers.length === 0) return null;

  return (
    <div>
      {providers.map(p => (
        <a
          key={p.name}
          href={`/_/api/admin/oauth/authorize/${encodeURIComponent(p.name)}?next=${encodeURIComponent(nextUrl)}`}
          style={{
            display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 10,
            width: '100%', padding: isHero ? '12px 18px' : '10px 16px', marginBottom: 8,
            borderRadius: isHero ? 18 : 10,
            border: `1px solid ${isHero ? 'color-mix(in srgb, var(--focus-ring) 34%, var(--glass-border))' : colors.BORDER}`,
            background: isHero
              ? 'linear-gradient(135deg, color-mix(in srgb, var(--glass-bg) 88%, white 12%), color-mix(in srgb, var(--input-bg) 72%, var(--focus-ring) 8%))'
              : 'var(--input-bg)',
            color: colors.TEXT_PRIMARY,
            fontSize, fontWeight: 600, fontFamily: 'var(--font-ui)',
            textDecoration: 'none', cursor: 'pointer',
            boxShadow: isHero ? '0 18px 40px rgba(0, 0, 0, 0.16), inset 0 1px 0 rgba(255, 255, 255, 0.08)' : undefined,
            transition: 'border-color 0.15s, background 0.15s, transform 0.15s, box-shadow 0.15s',
            ...(height ? { height } : {}),
          }}
          onMouseEnter={e => {
            e.currentTarget.style.borderColor = colors.ACCENT_BLUE;
            if (isHero) e.currentTarget.style.transform = 'translateY(-1px)';
          }}
          onMouseLeave={e => {
            e.currentTarget.style.borderColor = isHero ? 'color-mix(in srgb, var(--focus-ring) 34%, var(--glass-border))' : colors.BORDER;
            if (isHero) e.currentTarget.style.transform = 'translateY(0)';
          }}
        >
          <SafetyOutlined style={height ? { fontSize: 18 } : undefined} />
          Sign in with {p.display_name}
        </a>
      ))}
    </div>
  );
}
