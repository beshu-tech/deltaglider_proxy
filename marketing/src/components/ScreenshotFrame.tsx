import { useEffect, useId, useState } from 'react';

interface ScreenshotFrameProps {
  src: string;
  alt: string;
  caption?: string;
  priority?: boolean;
}

export function ScreenshotFrame({
  src,
  alt,
  caption,
  priority,
}: ScreenshotFrameProps): JSX.Element {
  const [isOpen, setIsOpen] = useState(false);
  const titleId = useId();
  const captionId = useId();
  const imageSrc =
    src.startsWith('http') || src.startsWith('/')
      ? src
      : `${import.meta.env.BASE_URL}${src}`;
  const lightboxCaption = caption ?? alt;

  useEffect(() => {
    if (!isOpen) return;

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setIsOpen(false);
    };
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    window.addEventListener('keydown', onKeyDown);

    return () => {
      document.body.style.overflow = previousOverflow;
      window.removeEventListener('keydown', onKeyDown);
    };
  }, [isOpen]);

  return (
    <figure className="group">
      <div className="overflow-hidden rounded-2xl border border-brand-300/40 bg-ink-950 shadow-2xl shadow-brand-950/20 ring-1 ring-white/10 dark:border-brand-400/30">
        <div className="flex items-center gap-2 border-b border-white/10 bg-ink-900 px-4 py-3">
          <span className="h-2.5 w-2.5 rounded-full bg-rose-400" />
          <span className="h-2.5 w-2.5 rounded-full bg-amber-300" />
          <span className="h-2.5 w-2.5 rounded-full bg-brand-300" />
          <span className="ml-3 truncate text-[11px] font-extrabold uppercase tracking-[0.2em] text-ink-400">
            live product
          </span>
        </div>
        <button
          type="button"
          className="relative block w-full cursor-zoom-in text-left"
          aria-label={`Open screenshot: ${lightboxCaption}`}
          onClick={() => setIsOpen(true)}
        >
          <img
            src={imageSrc}
            alt={alt}
            loading={priority ? 'eager' : 'lazy'}
            className="block h-auto w-full transition duration-500 group-hover:scale-[1.015]"
          />
          <span className="pointer-events-none absolute bottom-3 right-3 rounded-full border border-white/15 bg-ink-950/80 px-3 py-1 text-[11px] font-extrabold uppercase tracking-[0.16em] text-white/85 opacity-0 shadow-lg backdrop-blur transition group-hover:opacity-100">
            View larger
          </span>
        </button>
      </div>
      {caption && (
        <figcaption className="mt-4 rounded-2xl border border-ink-200/70 bg-white/75 p-4 shadow-sm shadow-ink-900/5 backdrop-blur-md dark:border-ink-700/70 dark:bg-ink-900/65">
          <div className="flex gap-4">
            <span
              aria-hidden
              className="mt-1 h-auto w-1 shrink-0 rounded-full bg-gradient-to-b from-brand-300 via-brand-500 to-brand-800"
            />
            <div className="min-w-0">
              <div className="text-[11px] font-extrabold uppercase tracking-[0.18em] text-brand-700 dark:text-brand-300">
                Product view
              </div>
              <p className="mt-1 max-w-prose text-sm leading-6 text-ink-600 dark:text-ink-300">
                {caption}
              </p>
            </div>
          </div>
        </figcaption>
      )}
      {isOpen && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-ink-950/90 p-4 backdrop-blur-md sm:p-8"
          role="dialog"
          aria-modal="true"
          aria-labelledby={titleId}
          aria-describedby={captionId}
          onClick={() => setIsOpen(false)}
        >
          <div
            className="flex max-h-full w-full max-w-7xl flex-col overflow-hidden rounded-3xl border border-white/15 bg-ink-950 shadow-2xl shadow-black/50"
            onClick={(event) => event.stopPropagation()}
          >
            <div className="flex items-center justify-between gap-4 border-b border-white/10 px-4 py-3 sm:px-5">
              <div className="min-w-0">
                <h2
                  id={titleId}
                  className="truncate text-sm font-extrabold uppercase tracking-[0.18em] text-brand-200"
                >
                  Product screenshot
                </h2>
                <p
                  id={captionId}
                  className="mt-1 max-w-4xl text-sm leading-6 text-ink-200"
                >
                  {lightboxCaption}
                </p>
              </div>
              <button
                type="button"
                className="shrink-0 rounded-full border border-white/15 px-3 py-1.5 text-sm font-bold text-white/80 transition hover:border-brand-300 hover:text-brand-200"
                onClick={() => setIsOpen(false)}
              >
                Close
              </button>
            </div>
            <div className="min-h-0 overflow-auto bg-black/40 p-3 sm:p-5">
              <img
                src={imageSrc}
                alt={alt}
                className="mx-auto block max-h-[78vh] w-auto max-w-full rounded-xl"
              />
            </div>
          </div>
        </div>
      )}
    </figure>
  );
}
