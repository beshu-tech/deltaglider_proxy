import { CONTACT_EMAIL } from '../seo/schema';

interface MailtoCTAProps {
  subject: string;
  label: string;
  variant?: 'primary' | 'secondary';
}

export function MailtoCTA({
  subject,
  label,
  variant = 'primary',
}: MailtoCTAProps): JSX.Element {
  const href = `mailto:${CONTACT_EMAIL}?subject=${encodeURIComponent(subject)}`;
  const base =
    'inline-flex items-center gap-2 rounded-lg px-5 py-3 font-semibold transition-colors';
  const styles =
    variant === 'primary'
      ? 'bg-brand-600 text-white hover:bg-brand-700 shadow-sm'
      : 'border border-ink-300 bg-white text-ink-800 hover:border-brand-400 hover:text-brand-700 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100 dark:hover:border-brand-300 dark:hover:text-brand-300';
  return (
    <a href={href} className={`${base} ${styles}`}>
      {label}
      <span aria-hidden>→</span>
    </a>
  );
}
