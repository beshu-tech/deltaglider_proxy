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
  const imageSrc =
    src.startsWith('http') || src.startsWith('/')
      ? src
      : `${import.meta.env.BASE_URL}${src}`;

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
        <img
          src={imageSrc}
          alt={alt}
          loading={priority ? 'eager' : 'lazy'}
          className="block h-auto w-full transition duration-500 group-hover:scale-[1.015]"
        />
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
    </figure>
  );
}
