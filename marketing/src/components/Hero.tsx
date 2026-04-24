import type { ReactNode } from 'react';

interface HeroProps {
  eyebrow?: string;
  headline: string;
  subhead: string;
  cta: ReactNode;
  illustration?: ReactNode;
}

export function Hero({
  eyebrow,
  headline,
  subhead,
  cta,
  illustration,
}: HeroProps): JSX.Element {
  return (
    <section className="mx-auto max-w-5xl px-6 pt-16 pb-12 sm:pt-24 sm:pb-16">
      <div className="grid gap-10 md:grid-cols-[1.2fr_1fr] md:items-center">
        <div>
          {eyebrow && (
            <div className="text-xs font-bold uppercase tracking-widest text-brand-600 dark:text-brand-300">
              {eyebrow}
            </div>
          )}
          <h1 className="mt-3 text-4xl sm:text-5xl font-extrabold tracking-tight text-ink-900 dark:text-ink-50 leading-[1.05]">
            {headline}
          </h1>
          <p className="mt-5 text-lg text-ink-600 dark:text-ink-300 leading-relaxed">
            {subhead}
          </p>
          <div className="mt-8 flex flex-wrap gap-3">{cta}</div>
        </div>
        {illustration && (
          <div className="rounded-xl border border-ink-200 dark:border-ink-700 overflow-hidden shadow-sm">
            {illustration}
          </div>
        )}
      </div>
    </section>
  );
}
