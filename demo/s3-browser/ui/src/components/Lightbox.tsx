import { useState, useCallback, useEffect, type ReactNode } from 'react';
import { useColors } from '../ThemeContext';

interface Props {
  /** The content to display (img, svg, etc.) */
  children: ReactNode;
  /** Caption shown below the content */
  caption?: string;
}

/** Click-to-zoom lightbox with caption. Works for images and SVG diagrams. */
export default function Lightbox({ children, caption }: Props) {
  const [open, setOpen] = useState(false);
  const colors = useColors();

  const handleClose = useCallback(() => setOpen(false), []);

  // ESC to close
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false); };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [open]);

  return (
    <>
      {/* Inline preview — click to expand */}
      <figure
        style={{ margin: '24px 0', cursor: 'zoom-in' }}
        onClick={() => setOpen(true)}
      >
        <div style={{
          border: `1px solid ${colors.BORDER}`,
          borderRadius: 8,
          overflow: 'hidden',
          background: colors.BG_CARD,
        }}>
          {children}
        </div>
        {caption && (
          <figcaption style={{
            textAlign: 'center',
            fontSize: 12,
            color: colors.TEXT_MUTED,
            fontFamily: 'var(--font-ui)',
            fontStyle: 'italic',
            marginTop: 8,
            lineHeight: 1.5,
          }}>
            {caption}
          </figcaption>
        )}
      </figure>

      {/* Full-screen overlay */}
      {open && (
        <div
          onClick={handleClose}
          style={{
            position: 'fixed',
            inset: 0,
            zIndex: 9999,
            background: 'rgba(0,0,0,0.85)',
            display: 'flex',
            flexDirection: 'column',
            alignItems: 'center',
            justifyContent: 'center',
            cursor: 'zoom-out',
            padding: 32,
          }}
        >
          <div
            onClick={e => e.stopPropagation()}
            style={{
              maxWidth: '90vw',
              maxHeight: '85vh',
              overflow: 'auto',
              borderRadius: 8,
              background: colors.BG_CARD,
              border: `1px solid ${colors.BORDER}`,
            }}
          >
            {children}
          </div>
          {caption && (
            <div style={{
              color: '#ccc',
              fontSize: 14,
              fontFamily: 'var(--font-ui)',
              fontStyle: 'italic',
              marginTop: 16,
              textAlign: 'center',
              maxWidth: '80vw',
            }}>
              {caption}
            </div>
          )}
          <div style={{
            position: 'absolute',
            top: 16,
            right: 24,
            color: '#888',
            fontSize: 12,
            fontFamily: 'var(--font-mono)',
          }}>
            ESC or click to close
          </div>
        </div>
      )}
    </>
  );
}
