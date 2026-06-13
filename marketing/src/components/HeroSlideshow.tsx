// HeroSlideshow.tsx — auto-advancing screenshot slideshow for the hero.
//
// Self-contained carousel (no external lib). Cross-fades through 5 admin-UI
// captures inside a faux app-window frame. Auto-advances every 4s, pauses on
// hover/focus, jumps via dot indicators. Respects prefers-reduced-motion.
//
// Hydrate lazily with Astro's client:visible — it's in the hero's right
// column, just at/below the fold, so the first frame is the LCP image (eager).

import { useEffect, useState } from 'react';

interface Slide {
  src: string;
  /** Short title shown in the faux title bar. */
  label: string;
  /** Full alt text for screen readers. */
  alt: string;
  /** Tiny mono caption under the frame. */
  caption: string;
}

// The 5 most striking + representative shots for "real, polished product".
// The square frame leads with the analytics money shot (a near-1:1 capture);
// the landscape captures that follow are top-anchored inside the square frame.
const SLIDES: Slide[] = [
  {
    src: '/screenshots/analytics-hero.jpg',
    label: 'Storage analytics',
    alt: 'Live storage analytics dashboard: 1,174% smaller on disk, 2.3 TB stored as 197 GB, per-bucket compression ratios across releases, db-archive, ml-models and downloads.',
    caption: 'Live savings, measured — not estimated',
  },
  {
    src: '/screenshots/filebrowser.jpg',
    label: 'Object browser',
    alt: 'DeltaGlider object browser showing a versioned zip with 97.2% storage savings, delta metadata and download/share controls.',
    caption: 'Transparent delta dedup — 97% saved, same S3 API',
  },
  {
    src: '/screenshots/iam.jpg',
    label: 'Identity & access',
    alt: 'IAM user management with fine-grained ABAC permissions mapped to users, groups and OAuth roles.',
    caption: 'Fine-grained ABAC — users, groups, OAuth roles',
  },
  {
    src: '/screenshots/object-replication.jpg',
    label: 'Replication',
    alt: 'Cross-bucket replication rules between S3, MinIO and Hetzner with pause and resume controls.',
    caption: 'Cross-bucket replication, pause & resume',
  },
  {
    src: '/screenshots/advanced_security.jpg',
    label: 'Encryption',
    alt: 'Advanced security configuration with AES-256-GCM at-rest encryption and customer-held keys.',
    caption: 'AES-256-GCM at rest — you hold the keys',
  },
];

const INTERVAL_MS = 4000;

function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(false);
  useEffect(() => {
    const mq = window.matchMedia('(prefers-reduced-motion: reduce)');
    setReduced(mq.matches);
    const onChange = (e: MediaQueryListEvent) => setReduced(e.matches);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, []);
  return reduced;
}

export default function HeroSlideshow() {
  const [active, setActive] = useState(0);
  const [paused, setPaused] = useState(false);
  const [lightbox, setLightbox] = useState(false);
  const reducedMotion = usePrefersReducedMotion();
  // Track which images have been requested so we never re-flash on revisit.
  const [loaded, setLoaded] = useState<Set<number>>(() => new Set([0]));

  const count = SLIDES.length;
  const go = (i: number) => setActive(((i % count) + count) % count);

  // Ensure the ACTIVE image (and the next, for a seamless cross-fade) are in
  // the render set. Jumping via a dot to a not-yet-loaded slide must still show
  // its image immediately — so we add both `active` and `active+1` here.
  useEffect(() => {
    const next = (active + 1) % count;
    setLoaded((prev) => {
      if (prev.has(active) && prev.has(next)) return prev;
      const copy = new Set(prev);
      copy.add(active);
      copy.add(next);
      return copy;
    });
  }, [active, count]);

  // Auto-advance. Disabled under reduced-motion, while paused (hover/focus),
  // or while the lightbox is open.
  useEffect(() => {
    if (reducedMotion || paused || lightbox) return;
    const id = window.setInterval(() => {
      setActive((a) => (a + 1) % count);
    }, INTERVAL_MS);
    return () => window.clearInterval(id);
  }, [reducedMotion, paused, lightbox, count]);

  // Escape closes the lightbox; lock body scroll while it's open.
  useEffect(() => {
    if (!lightbox) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setLightbox(false);
      else if (e.key === 'ArrowRight') setActive((a) => (a + 1) % count);
      else if (e.key === 'ArrowLeft') setActive((a) => (a - 1 + count) % count);
    };
    document.addEventListener('keydown', onKey);
    const prev = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    return () => {
      document.removeEventListener('keydown', onKey);
      document.body.style.overflow = prev;
    };
  }, [lightbox, count]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowRight') {
      e.preventDefault();
      go(active + 1);
    } else if (e.key === 'ArrowLeft') {
      e.preventDefault();
      go(active - 1);
    }
  };

  return (
    <div
      className={`hero-slideshow${reducedMotion ? ' is-reduced' : ''}`}
      role="group"
      aria-roledescription="carousel"
      aria-label="DeltaGlider admin UI screenshots"
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
      onFocusCapture={() => setPaused(true)}
      onBlurCapture={() => setPaused(false)}
      onKeyDown={onKeyDown}
    >
      {/* Stacked depth plates behind the frame for a layered, premium feel. */}
      <div className="hero-slideshow__stack" aria-hidden="true">
        <span className="hero-slideshow__plate hero-slideshow__plate--2" />
        <span className="hero-slideshow__plate hero-slideshow__plate--1" />
      </div>

      <div className="hero-slideshow__window">
        {/* Faux app chrome */}
        <div className="hero-slideshow__chrome">
          <span className="hero-slideshow__dots" aria-hidden="true">
            <i /><i /><i />
          </span>
          <span className="hero-slideshow__titlebar" aria-live="polite">
            <span className="hero-slideshow__app">DeltaGlider</span>
            <span className="hero-slideshow__sep">/</span>
            <span className="hero-slideshow__label">{SLIDES[active].label}</span>
          </span>
          <span className="hero-slideshow__chrome-spacer" aria-hidden="true" />
        </div>

        {/* Stage — click to open the lightbox; image zooms on hover. */}
        <button
          type="button"
          className="hero-slideshow__stage"
          aria-label={`View ${SLIDES[active].label} full size`}
          onClick={() => setLightbox(true)}
        >
          {SLIDES.map((slide, i) => {
            const isActive = i === active;
            return (
              <figure
                key={slide.src}
                className={`hero-slideshow__slide${isActive ? ' is-active' : ''}`}
                aria-hidden={!isActive}
              >
                {(loaded.has(i) || i === active || i === 0) && (
                  <img
                    src={slide.src}
                    alt={slide.alt}
                    width={1147}
                    height={1440}
                    loading={i === 0 ? 'eager' : 'lazy'}
                    // @ts-expect-error fetchpriority is valid HTML, not yet in React types
                    fetchpriority={i === 0 ? 'high' : 'auto'}
                    decoding="async"
                    draggable={false}
                  />
                )}
                <figcaption className="hero-slideshow__caption">
                  {slide.caption}
                </figcaption>
              </figure>
            );
          })}
          <span className="hero-slideshow__zoomhint" aria-hidden="true">⤢</span>
          <span className="hero-slideshow__sheen" aria-hidden="true" />
        </button>
      </div>

      {/* Prev / next arrows (replaces dot indicators). */}
      <div className="hero-slideshow__nav">
        <button
          type="button"
          className="hero-slideshow__arrow"
          aria-label="Previous screenshot"
          onClick={() => go(active - 1)}
        >
          <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 18l-6-6 6-6" /></svg>
        </button>
        <span className="hero-slideshow__counter" aria-live="polite">
          {active + 1}<span className="hero-slideshow__counter-sep">/</span>{count}
        </span>
        <button
          type="button"
          className="hero-slideshow__arrow"
          aria-label="Next screenshot"
          onClick={() => go(active + 1)}
        >
          <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 18l6-6-6-6" /></svg>
        </button>
      </div>

      {/* Lightbox */}
      {lightbox && (
        <div
          className="hero-lightbox"
          role="dialog"
          aria-modal="true"
          aria-label={`${SLIDES[active].label} — full size`}
          onClick={() => setLightbox(false)}
        >
          <button
            type="button"
            className="hero-lightbox__close"
            aria-label="Close"
            onClick={() => setLightbox(false)}
          >×</button>
          <button
            type="button"
            className="hero-lightbox__arrow hero-lightbox__arrow--prev"
            aria-label="Previous screenshot"
            onClick={(e) => { e.stopPropagation(); go(active - 1); }}
          >
            <svg viewBox="0 0 24 24" width="28" height="28" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 18l-6-6 6-6" /></svg>
          </button>
          <figure className="hero-lightbox__figure" onClick={(e) => e.stopPropagation()}>
            <img src={SLIDES[active].src} alt={SLIDES[active].alt} />
            <figcaption>{SLIDES[active].label} — {SLIDES[active].caption}</figcaption>
          </figure>
          <button
            type="button"
            className="hero-lightbox__arrow hero-lightbox__arrow--next"
            aria-label="Next screenshot"
            onClick={(e) => { e.stopPropagation(); go(active + 1); }}
          >
            <svg viewBox="0 0 24 24" width="28" height="28" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 18l6-6-6-6" /></svg>
          </button>
        </div>
      )}
    </div>
  );
}
