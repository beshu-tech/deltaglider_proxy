import type { ReactNode } from 'react';

interface FeatureCardProps {
  title: string;
  body: ReactNode;
  sourceLabel?: string;
  sourceHref?: string;
}

export function FeatureCard({
  title,
  body,
  sourceLabel,
  sourceHref,
}: FeatureCardProps): JSX.Element {
  return (
    <div className="rounded-xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/40 transition-shadow hover:shadow-md">
      <h3 className="text-lg font-extrabold text-ink-900 dark:text-ink-50">
        {title}
      </h3>
      <div className="mt-2 text-ink-600 dark:text-ink-300 leading-relaxed text-[15px]">
        {body}
      </div>
      {sourceHref && sourceLabel && (
        <a
          href={sourceHref}
          target="_blank"
          rel="noopener noreferrer"
          className="mt-4 inline-flex items-center gap-1 text-sm font-semibold text-brand-700 hover:text-brand-800 dark:text-brand-300 dark:hover:text-brand-200"
        >
          {sourceLabel}
          <span aria-hidden>↗</span>
        </a>
      )}
    </div>
  );
}
