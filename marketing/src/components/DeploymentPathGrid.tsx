import { Link } from 'react-router-dom';
import type { DeploymentPath, DeploymentPathAccent } from '../config/use-cases';
import { SiteIcon } from '../icons/SiteIcon';
import { LUCIDE_CARD } from '../icons/sizes';

export type { DeploymentPath } from '../config/use-cases';

const accentStyles: Record<
  DeploymentPathAccent,
  { bar: string; number: string; ringHover: string; iconBox: string }
> = {
  ember: {
    bar: 'from-amber-400 to-rose-500',
    number: 'text-amber-700 dark:text-amber-300/90',
    ringHover: 'group-hover:border-amber-400/50',
    iconBox: 'bg-amber-500/12 text-amber-800 dark:text-amber-200',
  },
  violet: {
    bar: 'from-fuchsia-400 to-violet-500',
    number: 'text-violet-700 dark:text-violet-300/90',
    ringHover: 'group-hover:border-fuchsia-400/45',
    iconBox: 'bg-violet-500/12 text-violet-800 dark:text-violet-200',
  },
  cyan: {
    bar: 'from-cyan-400 to-emerald-400',
    number: 'text-cyan-800 dark:text-cyan-200/90',
    ringHover: 'group-hover:border-cyan-400/45',
    iconBox: 'bg-cyan-500/12 text-cyan-900 dark:text-cyan-100',
  },
  sky: {
    bar: 'from-sky-400 to-brand-400',
    number: 'text-sky-800 dark:text-sky-200/80',
    ringHover: 'group-hover:border-sky-400/45',
    iconBox: 'bg-sky-500/12 text-sky-900 dark:text-sky-100',
  },
  emerald: {
    bar: 'from-lime-400 to-emerald-500',
    number: 'text-emerald-800 dark:text-emerald-200/80',
    ringHover: 'group-hover:border-lime-400/45',
    iconBox: 'bg-lime-500/12 text-emerald-900 dark:text-emerald-100',
  },
};

export function DeploymentPathGrid({ paths }: { paths: readonly DeploymentPath[] }): JSX.Element {
  return (
    <div>
      <div className="mx-auto flex max-w-5xl flex-wrap justify-center gap-5">
        {paths.map((p, i) => {
          const a = accentStyles[p.accent];
          const n = String(i + 1).padStart(2, '0');
          return (
            <Link
              key={p.to}
              to={p.to}
              className={[
                'group relative flex w-full min-h-[12.5rem] flex-col',
                'sm:max-w-sm sm:shrink-0 sm:basis-80',
                'rounded-2xl border border-ink-200/80 bg-gradient-to-b from-white to-ink-50/90 p-px',
                'shadow-sm shadow-ink-900/5',
                'transition duration-200',
                'hover:-translate-y-0.5 hover:shadow-lg hover:shadow-ink-900/10',
                'focus:outline-none focus-visible:ring-2 focus-visible:ring-brand-500 focus-visible:ring-offset-2 focus-visible:ring-offset-white',
                'dark:from-ink-900 dark:to-ink-950/90 dark:border-ink-600/60 dark:shadow-black/20',
                'dark:focus-visible:ring-offset-ink-900',
                a.ringHover,
              ].join(' ')}
              aria-label={`${p.who}: read the full use case.`}
            >
              <div className="flex h-full flex-col overflow-hidden rounded-[0.9rem] bg-white/95 p-5 dark:bg-ink-900/85">
                <div className={`h-0.5 w-12 rounded-full bg-gradient-to-r ${a.bar}`} aria-hidden />
                <div className="mt-4 flex items-start justify-between gap-2">
                  <span className="min-w-0 text-[11px] font-extrabold uppercase tracking-[0.16em] text-ink-500 dark:text-ink-400">
                    {p.voice}
                  </span>
                  <div className="flex shrink-0 items-center gap-1.5">
                    <div
                      className={['rounded-lg p-1.5', a.iconBox].join(' ')}
                      aria-hidden
                    >
                      <SiteIcon icon={p.icon} className={LUCIDE_CARD} />
                    </div>
                    <span
                      className={['font-mono text-xs font-bold tabular-nums', a.number].join(
                        ' '
                      )}
                      aria-hidden
                    >
                      {n}
                    </span>
                  </div>
                </div>
                <h3 className="mt-3 text-lg font-extrabold leading-snug tracking-tight text-ink-900 dark:text-ink-50 sm:text-[1.15rem]">
                  {p.who}
                </h3>
                <p className="mt-2 min-h-0 flex-1 text-sm leading-relaxed text-ink-600 dark:text-ink-300">
                  {p.payoff}
                </p>
                <div
                  className="mt-3 flex select-none items-center justify-end text-lg font-semibold leading-none text-brand-600/70 transition group-hover:translate-x-0.5 group-hover:text-brand-500 dark:text-brand-300/70 dark:group-hover:text-brand-300"
                  aria-hidden
                >
                  ↗
                </div>
              </div>
            </Link>
          );
        })}
      </div>
    </div>
  );
}
