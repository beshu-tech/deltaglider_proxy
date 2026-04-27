import { CircleCheck } from 'lucide-react';
import { SiteIcon } from '../icons/SiteIcon';
import { LUCIDE_STROKE } from '../icons/sizes';

const ROWS: readonly { lead: string; rest: string }[] = [
  {
    lead: 'Smaller on disk',
    rest: 'Repeated and versioned binaries as xdelta3—apps still see plain S3 API semantics.',
  },
  {
    lead: 'Access & security',
    rest: 'IAM, OAuth, ABAC, zero trust encryption at rest.',
  },
  {
    lead: 'How you run it',
    rest: 'Quotas, replication, metrics, audit trail, and configuration through one admin surface.',
  },
];

export function LandingHeroSubhead(): JSX.Element {
  return (
    <div className="flex flex-col gap-6 text-ink-600 dark:text-ink-300 sm:gap-5">
      <p className="m-0 text-lg leading-relaxed sm:text-[1.125rem] sm:leading-8">
        <span className="font-semibold text-ink-900 dark:text-ink-100">
          Identity, Policy, Ops, Security.
        </span>{' '}
        S3 in, S3 out.
      </p>
      <ul
        className="m-0 list-none space-y-3 p-0 sm:space-y-2.5"
        aria-label="Key capabilities at a glance"
      >
        {ROWS.map((row) => (
          <li
            key={row.lead}
            className="flex gap-3 text-[0.95rem] leading-6 sm:text-base sm:leading-7"
          >
            <span
              className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center text-brand-600 dark:text-brand-400"
              aria-hidden
            >
              <SiteIcon
                icon={CircleCheck}
                className="h-5 w-5"
                strokeWidth={LUCIDE_STROKE + 0.1}
              />
            </span>
            <span>
              <span className="font-bold text-ink-900 dark:text-ink-50">{row.lead}.</span>
              {` ${row.rest}`}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}
