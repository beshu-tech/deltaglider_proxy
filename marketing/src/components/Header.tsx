import { Link } from 'react-router-dom';
import { REPO_URL } from '../seo/schema';

export function Header(): JSX.Element {
  return (
    <header className="sticky top-0 z-10 border-b border-ink-200/60 bg-white/85 backdrop-blur dark:border-ink-700/60 dark:bg-ink-950/85">
      <div className="mx-auto flex max-w-6xl items-center justify-between px-6 py-4">
        <Link
          to="/"
          className="font-extrabold text-lg tracking-tight text-ink-900 dark:text-ink-50 hover:text-brand-600 dark:hover:text-brand-300 transition-colors"
        >
          DeltaGlider <span className="text-brand-500">Proxy</span>
        </Link>
        <nav className="hidden items-center gap-5 text-sm font-semibold md:flex">
          <Link
            to="/regulated/"
            className="text-ink-700 hover:text-brand-600 dark:text-ink-200 dark:hover:text-brand-300"
          >
            Regulated
          </Link>
          <Link
            to="/artifact-storage/"
            className="text-ink-700 hover:text-brand-600 dark:text-ink-200 dark:hover:text-brand-300"
          >
            Artifact storage
          </Link>
          <Link
            to="/s3-to-hetzner-wasabi/"
            className="text-ink-700 hover:text-brand-600 dark:text-ink-200 dark:hover:text-brand-300"
          >
            AWS migration
          </Link>
          <Link
            to="/multi-cloud-control-plane/"
            className="text-ink-700 hover:text-brand-600 dark:text-ink-200 dark:hover:text-brand-300"
          >
            Multi-cloud
          </Link>
          <Link
            to="/minio-migration/"
            className="text-ink-700 hover:text-brand-600 dark:text-ink-200 dark:hover:text-brand-300"
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
        <details className="relative md:hidden">
          <summary className="list-none rounded-md border border-ink-300 bg-white px-3 py-1.5 text-sm font-bold text-ink-800 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100">
            Menu
          </summary>
          <div className="absolute right-0 mt-3 w-56 rounded-xl border border-ink-200 bg-white p-2 shadow-xl dark:border-ink-700 dark:bg-ink-900">
            <Link
              to="/regulated/"
              className="block rounded-lg px-3 py-2 text-sm font-semibold text-ink-700 hover:bg-ink-100 dark:text-ink-200 dark:hover:bg-ink-800"
            >
              Regulated workloads
            </Link>
            <Link
              to="/artifact-storage/"
              className="block rounded-lg px-3 py-2 text-sm font-semibold text-ink-700 hover:bg-ink-100 dark:text-ink-200 dark:hover:bg-ink-800"
            >
              Artifact storage
            </Link>
            <Link
              to="/s3-to-hetzner-wasabi/"
              className="block rounded-lg px-3 py-2 text-sm font-semibold text-ink-700 hover:bg-ink-100 dark:text-ink-200 dark:hover:bg-ink-800"
            >
              AWS migration
            </Link>
            <Link
              to="/multi-cloud-control-plane/"
              className="block rounded-lg px-3 py-2 text-sm font-semibold text-ink-700 hover:bg-ink-100 dark:text-ink-200 dark:hover:bg-ink-800"
            >
              Multi-cloud
            </Link>
            <Link
              to="/minio-migration/"
              className="block rounded-lg px-3 py-2 text-sm font-semibold text-ink-700 hover:bg-ink-100 dark:text-ink-200 dark:hover:bg-ink-800"
            >
              MinIO migration
            </Link>
            <a
              href={REPO_URL}
              target="_blank"
              rel="noopener noreferrer"
              className="block rounded-lg px-3 py-2 text-sm font-semibold text-brand-700 hover:bg-brand-50 dark:text-brand-300 dark:hover:bg-ink-800"
            >
              GitHub
            </a>
          </div>
        </details>
      </div>
    </header>
  );
}
