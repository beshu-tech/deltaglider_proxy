import { Link } from 'react-router-dom';
import { CONTACT_EMAIL, ORG_NAME, REPO_URL } from '../seo/schema';

export function Footer(): JSX.Element {
  return (
    <footer className="mt-24 border-t border-ink-200/60 bg-white/60 dark:border-ink-700/60 dark:bg-ink-900/60">
      <div className="mx-auto max-w-5xl px-6 py-10 grid gap-8 sm:grid-cols-3 text-sm">
        <div>
          <div className="font-extrabold text-base text-ink-900 dark:text-ink-50">
            DeltaGlider <span className="text-brand-500">Proxy</span>
          </div>
          <p className="mt-2 text-ink-600 dark:text-ink-400">
            An S3-compatible proxy with transparent delta compression.
          </p>
          <p className="mt-3 text-ink-500 dark:text-ink-500">
            © {new Date().getFullYear()} {ORG_NAME}
          </p>
        </div>
        <div>
          <div className="font-semibold text-ink-800 dark:text-ink-200">
            Use cases
          </div>
          <ul className="mt-2 space-y-1.5 text-ink-600 dark:text-ink-400">
            <li>
              <Link to="/regulated/" className="hover:text-brand-600 dark:hover:text-brand-300">
                Regulated workloads
              </Link>
            </li>
            <li>
              <Link to="/versioning/" className="hover:text-brand-600 dark:hover:text-brand-300">
                Artifact versioning
              </Link>
            </li>
            <li>
              <Link to="/minio-migration/" className="hover:text-brand-600 dark:hover:text-brand-300">
                MinIO migration
              </Link>
            </li>
          </ul>
        </div>
        <div>
          <div className="font-semibold text-ink-800 dark:text-ink-200">
            Project
          </div>
          <ul className="mt-2 space-y-1.5 text-ink-600 dark:text-ink-400">
            <li>
              <a
                href={REPO_URL}
                target="_blank"
                rel="noopener noreferrer"
                className="hover:text-brand-600 dark:hover:text-brand-300"
              >
                GitHub repository
              </a>
            </li>
            <li>
              <a
                href={`${REPO_URL}/blob/main/LICENSE`}
                target="_blank"
                rel="noopener noreferrer"
                className="hover:text-brand-600 dark:hover:text-brand-300"
              >
                License
              </a>
            </li>
            <li>
              <a
                href={`mailto:${CONTACT_EMAIL}`}
                className="hover:text-brand-600 dark:hover:text-brand-300"
              >
                {CONTACT_EMAIL}
              </a>
            </li>
          </ul>
        </div>
      </div>
    </footer>
  );
}
