import { useColors } from '../ThemeContext';
import { SPACE } from './shared-styles';

/**
 * Centered tab header with icon, title, and description.
 * Used at the top of every admin settings tab for consistent wayfinding.
 */

interface Props {
  icon: React.ReactNode;
  title: string;
  description: string;
  /** Receives the node at the right of the header that <PageActions>
   *  portals the page's primary action into. */
  actionsRef?: (node: HTMLDivElement | null) => void;
}

/**
 * No page-level save-model badge: a page often mixes staged editors and
 * buttons that act at once, so a single page label contradicted some of its
 * own actions (#92 item 13). Each staged editor's dirty bar carries a
 * "Review & apply" button instead; immediate buttons say what they do.
 */
export default function TabHeader({ icon, title, description, actionsRef }: Props) {
  const colors = useColors();

  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        flexWrap: 'wrap',
        gap: SPACE.sm,
        rowGap: SPACE.xs,
        padding: `${SPACE.sm}px ${SPACE.lg}px`,
        borderBottom: `1px solid ${colors.BORDER}`,
        marginBottom: 0,
      }}
    >
      <div
        aria-hidden="true"
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          width: 28,
          height: 28,
          borderRadius: 8,
          background: `${colors.ACCENT_BLUE}12`,
          border: `1px solid ${colors.ACCENT_BLUE}25`,
          fontSize: 14,
          color: colors.ACCENT_BLUE,
          flexShrink: 0,
        }}
      >
        {icon}
      </div>
      <div
        style={{
          minWidth: 0,
          flex: 1,
        }}
      >
        <div
          role="heading"
          aria-level={2}
          style={{
            fontSize: 16,
            fontWeight: 700,
            color: colors.TEXT_PRIMARY,
            fontFamily: 'var(--font-ui)',
            letterSpacing: '-0.01em',
            lineHeight: 1.15,
          }}
        >
          {title}
        </div>
        <div
          style={{
            fontSize: 12,
            color: colors.TEXT_MUTED,
            fontFamily: 'var(--font-ui)',
            marginTop: 2,
            maxWidth: 760,
            lineHeight: 1.35,
          }}
        >
          {description}
        </div>
      </div>
      {/* The page's ONE primary action (see PageActions). */}
      {actionsRef && <div ref={actionsRef} data-testid="page-actions" style={{ display: 'flex', gap: 8, alignItems: 'center', flexShrink: 0 }} />}
    </div>
  );
}
