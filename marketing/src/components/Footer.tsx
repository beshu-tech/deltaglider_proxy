import { Link } from 'react-router-dom';
import { CONTACT_EMAIL, DOCS_PATH, ORG_NAME, REPO_URL } from '../seo/schema';

export function Footer(): JSX.Element {
  return (
    <footer className="mt-24 border-t border-ink-800 bg-ink-950 text-ink-100">
      <div className="mx-auto max-w-6xl px-6 py-12">
        <div className="grid gap-10 lg:grid-cols-[1.25fr_2fr]">
          <div>
            <div className="text-2xl font-extrabold tracking-tight">
              DeltaGlider <span className="text-brand-300">Proxy</span>
            </div>
            <p className="mt-4 max-w-md text-sm leading-relaxed text-ink-200">
              S3-compatible storage efficiency with IAM, OAuth, quotas,
              replication, metrics, audit, and encryption controls.
            </p>
            <div className="mt-6 rounded-2xl border border-brand-300/40 bg-brand-300/15 p-4 shadow-lg shadow-brand-950/20">
              <div className="text-xs font-extrabold uppercase tracking-widest text-brand-200">
                Built by
              </div>
              <a
                href="https://beshu.tech"
                target="_blank"
                rel="noopener noreferrer"
                className="mt-1 inline-flex text-lg font-extrabold text-white hover:text-brand-200"
              >
                Beshu Tech
                <span aria-hidden className="ml-1">
                  →
                </span>
              </a>
              <p className="mt-2 text-sm leading-relaxed text-ink-200">
                Infrastructure software for secure search, observability, and
                storage operations.
              </p>
            </div>
          </div>

          <div className="grid gap-8 sm:grid-cols-3">
            <div>
              <div className="font-extrabold text-white">Use cases</div>
              <ul className="mt-3 space-y-2 text-sm font-semibold text-ink-200">
                <li>
                  <Link to="/regulated/" className="hover:text-brand-200">
                    Regulated workloads
                  </Link>
                </li>
                <li>
                  <Link to="/artifact-storage/" className="hover:text-brand-200">
                    Artifact storage
                  </Link>
                </li>
                <li>
                  <Link to="/s3-to-hetzner-wasabi/" className="hover:text-brand-200">
                    S3 to Hetzner / Wasabi
                  </Link>
                </li>
                <li>
                  <Link to="/multi-cloud-control-plane/" className="hover:text-brand-200">
                    Multi-cloud control plane
                  </Link>
                </li>
                <li>
                  <Link to="/minio-migration/" className="hover:text-brand-200">
                    MinIO migration
                  </Link>
                </li>
              </ul>
            </div>
            <div>
              <div className="font-extrabold text-white">Product</div>
              <ul className="mt-3 space-y-2 text-sm font-semibold text-ink-200">
                <li>
                  <Link to={DOCS_PATH} className="hover:text-brand-200">
                    Product docs
                  </Link>
                </li>
                <li>
                  <a
                    href={REPO_URL}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="hover:text-brand-200"
                  >
                    GitHub repository
                  </a>
                </li>
                <li>
                  <a
                    href={`${REPO_URL}/blob/main/LICENSE`}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="hover:text-brand-200"
                  >
                    License
                  </a>
                </li>
                <li>
                  <a
                    href={`mailto:${CONTACT_EMAIL}`}
                    className="hover:text-brand-200"
                  >
                    Contact engineering
                  </a>
                </li>
              </ul>
            </div>
            <div>
              <div className="font-extrabold text-white">Company</div>
              <ul className="mt-3 space-y-2 text-sm font-semibold text-ink-200">
                <li>
                  <Link to="/about/" className="hover:text-brand-200">
                    About
                  </Link>
                </li>
                <li>
                  <Link to="/privacy/" className="hover:text-brand-200">
                    Privacy
                  </Link>
                </li>
                <li>
                  <Link to="/terms/" className="hover:text-brand-200">
                    Terms
                  </Link>
                </li>
              </ul>
            </div>
          </div>
        </div>

        <div className="mt-10 flex flex-col gap-3 border-t border-white/15 pt-6 text-xs font-semibold text-ink-300 sm:flex-row sm:items-center sm:justify-between">
          <p>
            © {new Date().getFullYear()} {ORG_NAME}. All rights reserved.
          </p>
          <p>S3-compatible storage efficiency for enterprise operators.</p>
        </div>
      </div>
    </footer>
  );
}
