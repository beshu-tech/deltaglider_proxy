import type { ReactNode } from 'react';

interface SectionProps {
  id?: string;
  eyebrow?: string;
  title: string;
  intro?: ReactNode;
  children: ReactNode;
}

export function Section({
  id,
  eyebrow,
  title,
  intro,
  children,
}: SectionProps): JSX.Element {
  return (
    <section id={id} className="mx-auto max-w-6xl px-6 py-14 sm:py-20">
      {eyebrow && (
        <div className="text-xs font-bold uppercase tracking-widest text-brand-600 dark:text-brand-300">
          {eyebrow}
        </div>
      )}
      <h2 className="mt-2 text-3xl sm:text-4xl font-extrabold tracking-tight text-ink-900 dark:text-ink-50">
        {title}
      </h2>
      {intro && (
        <div className="mt-4 max-w-3xl text-lg text-ink-600 dark:text-ink-300 leading-relaxed">
          {intro}
        </div>
      )}
      <div className="mt-10">{children}</div>
    </section>
  );
}
