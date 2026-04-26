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
  const sentenceBoundary = headline.indexOf('. ');
  const lead =
    sentenceBoundary >= 0 ? headline.slice(0, sentenceBoundary + 1) : headline;
  const rest =
    sentenceBoundary >= 0 ? headline.slice(sentenceBoundary + 2) : '';

  return (
    <section className="cyber-fishnet-hero relative isolate overflow-hidden">
      <div className="absolute inset-0 z-0 bg-[radial-gradient(circle_at_top_left,rgba(20,184,166,0.22),transparent_32rem),linear-gradient(135deg,rgba(255,255,255,0.85),rgba(236,253,248,0.48),rgba(248,250,252,0.9))] dark:bg-[radial-gradient(circle_at_top_left,rgba(45,212,191,0.16),transparent_34rem),linear-gradient(135deg,rgba(15,23,42,0.95),rgba(19,78,74,0.24),rgba(15,23,42,0.9))]" />
      <div className="relative z-10 mx-auto grid max-w-6xl gap-10 px-6 pt-16 pb-12 sm:pt-24 sm:pb-16 lg:grid-cols-[0.9fr_1.1fr] lg:items-center">
        <div>
          {eyebrow && (
            <div className="text-xs font-bold uppercase tracking-widest text-brand-600 dark:text-brand-300">
              {eyebrow}
            </div>
          )}
          <h1 className="mt-3 max-w-4xl text-5xl font-extrabold tracking-tight text-ink-900 dark:text-ink-50 sm:text-6xl lg:text-7xl leading-[0.95]">
            <span className="bg-gradient-to-r from-brand-700 via-brand-500 to-ink-900 bg-clip-text text-transparent dark:from-brand-200 dark:via-brand-300 dark:to-ink-50">
              {lead}
            </span>
            {rest && (
              <>
                <br />
                <span>{rest}</span>
              </>
            )}
          </h1>
          <p className="mt-6 max-w-2xl text-lg text-ink-600 dark:text-ink-300 leading-relaxed sm:text-xl">
            {subhead}
          </p>
          <div className="mt-8 flex flex-wrap gap-3">{cta}</div>
        </div>
        {illustration && <div>{illustration}</div>}
      </div>
    </section>
  );
}
