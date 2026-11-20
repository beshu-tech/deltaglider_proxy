import { Button } from 'antd';
import { ArrowLeftOutlined, QuestionCircleOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { useIsNarrow } from '../useIsNarrow';
import { BREAKPOINT } from './shared-styles';

interface Props {
  title: string;
  onBack: () => void;
  /** Optional left-edge control before the back button: the mobile menu
   *  button, which sits on the left in the object browser too. */
  leading?: React.ReactNode;
  /** Optional right-side content (e.g. backup buttons) */
  extra?: React.ReactNode;
  /** Open the keyboard-shortcuts help modal (renders a help icon when set). */
  onShowShortcuts?: () => void;
  accountMenu?: React.ReactNode;
}

/** Shared header bar for full-screen views (Admin, Docs) */
export default function FullScreenHeader({ title, onBack, leading, extra, onShowShortcuts, accountMenu }: Props) {
  const colors = useColors();
  // On a phone the header holds the menu button, "Browser", help and the
  // account menu; the letter-spaced title then wraps onto two lines. The page
  // header below names the page, so the bar drops the title there.
  const phone = useIsNarrow(BREAKPOINT.phone);
  return (
    <div className="dg-fullscreen-header" style={{
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      padding: phone ? '0 8px' : '0 24px',
      gap: 8,
      height: 52,
      borderBottom: '1px solid var(--chrome-border)',
      background: 'var(--chrome-bg)',
      flexShrink: 0,
    }}>
      {/* Left: optional menu button + back button */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
      {leading}
      <Button
        type="text"
        icon={<ArrowLeftOutlined />}
        onClick={onBack}
        style={{
          color: 'var(--chrome-text)',
          fontWeight: 500,
          fontFamily: 'var(--font-ui)',
        }}
      >
        Browser
      </Button>
      </div>

      {/* Center: branding + section title */}
      {!phone && <div style={{
        display: 'flex',
        alignItems: 'baseline',
        gap: 8,
        userSelect: 'none',
      }}>
        <button
          type="button"
          className="btn-reset hide-mobile dg-wordmark dg-hover-text"
          onClick={onBack}
          title="Open the file browser"
          style={{ fontSize: 13, color: 'var(--chrome-muted)' }}
        >
          DeltaGlider
        </button>
        <span className="hide-mobile" style={{
          width: 1,
          height: 14,
          background: colors.BORDER,
          display: 'inline-block',
          verticalAlign: 'middle',
          position: 'relative',
          top: 1,
        }} />
        <span className="dg-wordmark-product" style={{
          fontSize: 13,
          whiteSpace: 'nowrap',
          color: 'var(--chrome-accent)',
        }}>
          {title}
        </span>
      </div>}

      {/* Right: optional actions + account menu */}
      <div style={{ minWidth: phone ? 0 : 100, display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 4 }}>
        {extra}
        {onShowShortcuts && (
          <Button
            type="text"
            icon={<QuestionCircleOutlined />}
            title="Keyboard shortcuts (press ?)"
            aria-label="Keyboard shortcuts"
            onClick={onShowShortcuts}
            style={{ color: 'var(--chrome-muted)' }}
          />
        )}
        {accountMenu}
      </div>
    </div>
  );
}
