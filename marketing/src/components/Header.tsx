import { Link } from 'react-router-dom';
import { REPO_URL } from '../seo/schema';

export function Header(): JSX.Element {
  return (
    <header className="border-b border-ink-200/60 bg-white/80 backdrop-blur dark:border-ink-700/60 dark:bg-ink-900/80 sticky top-0 z-10">
      <div className="mx-auto flex max-w-5xl items-center justify-between px-6 py-4">
        <Link
          to="/"
          className="font-extrabold text-lg tracking-tight text-ink-900 dark:text-ink-50 hover:text-brand-600 dark:hover:text-brand-300 transition-colors"
        >
          DeltaGlider <span className="text-brand-500">Proxy</span>
        </Link>
        <nav className="flex items-center gap-5 text-sm font-semibold">
          <Link
            to="/regulated/"
            className="text-ink-700 hover:text-brand-600 dark:text-ink-200 dark:hover:text-brand-300 hidden sm:inline"
          >
            Regulated
          </Link>
          <Link
            to="/versioning/"
            className="text-ink-700 hover:text-brand-600 dark:text-ink-200 dark:hover:text-brand-300 hidden sm:inline"
          >
            Versioning
          </Link>
          <Link
            to="/minio-migration/"
            className="text-ink-700 hover:text-brand-600 dark:text-ink-200 dark:hover:text-brand-300 hidden sm:inline"
          >
            MinIO migration
          </Link>
          <a
            href={REPO_URL}
            target="_blank"
            rel="noopener noreferrer"
            className="rounded-md border border-ink-300 bg-white px-3 py-1.5 text-ink-800 hover:border-brand-400 hover:text-brand-700 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100 dark:hover:border-brand-300 dark:hover:text-brand-300"
          >
            GitHub
          </a>
        </nav>
      </div>
    </header>
  );
}
