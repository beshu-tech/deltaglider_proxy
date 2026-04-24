import type { ReactNode } from 'react';

interface RoadmapRibbonProps {
  title: string;
  body: ReactNode;
  href?: string;
  hrefLabel?: string;
}

export function RoadmapRibbon({
  title,
  body,
  href,
  hrefLabel,
}: RoadmapRibbonProps): JSX.Element {
  return (
    <div className="rounded-xl border border-amber-300 bg-amber-50 p-5 dark:border-amber-700/60 dark:bg-amber-950/30">
      <div className="flex items-start gap-3">
        <span className="mt-0.5 inline-flex items-center rounded-md bg-amber-200 px-2 py-0.5 text-xs font-bold uppercase tracking-wider text-amber-900 dark:bg-amber-700/50 dark:text-amber-100">
          Coming soon
        </span>
        <div className="flex-1">
          <h4 className="font-bold text-ink-900 dark:text-ink-50">{title}</h4>
          <p className="mt-1 text-sm text-ink-700 dark:text-ink-300">{body}</p>
          {href && hrefLabel && (
            <a
              href={href}
              target="_blank"
              rel="noopener noreferrer"
              className="mt-2 inline-block text-sm font-semibold text-amber-800 hover:text-amber-900 dark:text-amber-300 dark:hover:text-amber-200"
            >
              {hrefLabel} ↗
            </a>
          )}
        </div>
      </div>
    </div>
  );
}
