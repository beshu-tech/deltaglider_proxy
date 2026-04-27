import { useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { USE_CASE_PATHS, siteNavIcon } from '../config/use-cases';
import { DOCS_PATH, REPO_URL } from '../seo/schema';
import { SiteIcon } from '../icons/SiteIcon';
import { LUCIDE_LG, LUCIDE_MEGA, LUCIDE_MD, LUCIDE_SM } from '../icons/sizes';

const THEME_STORAGE_KEY = 'dgp-marketing-theme';

type Theme = 'light' | 'dark';

function getCurrentTheme(): Theme {
  if (typeof document === 'undefined') return 'light';
  return document.documentElement.dataset['theme'] === 'dark' ? 'dark' : 'light';
}

function applyTheme(theme: Theme): void {
  document.documentElement.dataset['theme'] = theme;
  window.localStorage.setItem(THEME_STORAGE_KEY, theme);
}

function SunOutlinedIcon(): JSX.Element {
  return (
    <svg viewBox="64 64 896 896" width="1em" height="1em" fill="currentColor" aria-hidden>
      <path d="M512 704a192 192 0 1 0 0-384 192 192 0 0 0 0 384Zm0-64a128 128 0 1 1 0-256 128 128 0 0 1 0 256ZM480 96h64v128h-64V96Zm0 704h64v128h-64V800ZM224 480v64H96v-64h128Zm704 0v64H800v-64h128ZM241.7 196.7l90.5 90.5-45.3 45.3-90.5-90.5 45.3-45.3Zm495 495 90.5 90.5-45.3 45.3-90.5-90.5 45.3-45.3Zm45.2-495 45.3 45.3-90.5 90.5-45.3-45.3 90.5-90.5Zm-495 495 45.3 45.3-90.5 90.5-45.3-45.3 90.5-90.5Z" />
    </svg>
  );
}

function MoonOutlinedIcon(): JSX.Element {
  return (
    <svg viewBox="64 64 896 896" width="1em" height="1em" fill="currentColor" aria-hidden>
      <path d="M848 729.7A408 408 0 0 1 294.3 176a32 32 0 0 1 37.5 41.8A344 344 0 0 0 806.2 692.2a32 32 0 0 1 41.8 37.5ZM512 896a384 384 0 0 0 267.9-109.1 408 408 0 0 1-542.8-542.8A384 384 0 0 0 512 896Z" />
    </svg>
  );
}

function ThemeToggle(): JSX.Element {
  const [theme, setTheme] = useState<Theme>('light');

  useEffect(() => {
    setTheme(getCurrentTheme());
  }, []);

  const nextTheme = theme === 'dark' ? 'light' : 'dark';

  return (
    <button
      type="button"
      onClick={() => {
        applyTheme(nextTheme);
        setTheme(nextTheme);
      }}
      className="inline-flex h-8 w-8 items-center justify-center rounded-md text-ink-500 transition hover:bg-ink-100 hover:text-brand-700 dark:text-ink-300 dark:hover:bg-ink-800 dark:hover:text-brand-300"
      aria-label={`Switch to ${nextTheme} theme`}
      title={`Switch to ${nextTheme} theme`}
    >
      {theme === 'dark' ? <MoonOutlinedIcon /> : <SunOutlinedIcon />}
    </button>
  );
}

const megaIconClass =
  'flex h-10 w-10 shrink-0 items-center justify-center rounded-xl border border-white/10 bg-gradient-to-b from-white/[0.1] to-white/[0.03] text-brand-200 transition group-hover/item:border-brand-200/35 group-hover/item:from-white/[0.12] group-hover/item:text-white';

function UseCasesMegaMenu(): JSX.Element {
  const [open, setOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;

    const onPointerDown = (event: PointerEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) return;
      if (menuRef.current?.contains(target)) return;
      setOpen(false);
    };

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false);
    };

    document.addEventListener('pointerdown', onPointerDown);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [open]);

  return (
    <div ref={menuRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        className="inline-flex items-center gap-2 rounded-lg px-3 py-2 text-ink-700 transition hover:bg-ink-100 hover:text-brand-700 dark:text-ink-200 dark:hover:bg-ink-800 dark:hover:text-brand-300"
        aria-expanded={open}
        aria-haspopup="menu"
        aria-label="Use cases"
      >
        <SiteIcon icon={siteNavIcon.useCases} className={LUCIDE_MD} />
        <span>Use cases</span>
      </button>
      {open && (
        <div
          className="absolute right-0 top-full z-20 mt-4 w-[min(760px,calc(100vw-3rem))] overflow-hidden rounded-3xl border border-brand-300/20 bg-ink-950 p-3 text-white shadow-2xl shadow-ink-950/30"
          role="menu"
        >
          <div className="border-b border-white/10 px-4 pb-3 pt-2">
            <div className="flex items-center gap-2 text-[11px] font-black uppercase tracking-[0.24em] text-brand-300">
              <SiteIcon icon={siteNavIcon.useCases} className={LUCIDE_SM} />
              Use cases
            </div>
            <p className="mt-1 max-w-xl text-sm font-semibold leading-6 text-ink-300">
              Start with the storage problem. Each path maps to a concrete deployment shape.
            </p>
          </div>
          <div className="grid gap-1 py-3 sm:grid-cols-2">
            {USE_CASE_PATHS.map((item) => {
              return (
                <Link
                  key={item.to}
                  to={item.to}
                  onClick={() => setOpen(false)}
                  className="group/item flex gap-3 rounded-2xl border border-transparent p-3.5 text-left transition hover:border-brand-300/40 hover:bg-white/[0.06]"
                  role="menuitem"
                >
                  <div className={megaIconClass} aria-hidden>
                    <SiteIcon icon={item.icon} className={LUCIDE_MEGA} />
                  </div>
                  <div className="min-w-0">
                    <div className="font-extrabold text-white group-hover/item:text-brand-200">
                      {item.navLabel}
                    </div>
                    <p className="mt-1.5 text-sm font-medium leading-6 text-ink-300">
                      {item.summary}
                    </p>
                  </div>
                </Link>
              );
            })}
          </div>
          <div className="rounded-2xl border border-brand-300/40 bg-gradient-to-r from-brand-300 to-cyan-200 p-4 text-ink-950">
            <Link
              to={DOCS_PATH}
              onClick={() => setOpen(false)}
              className="inline-flex items-center gap-2.5 text-sm font-black text-ink-950 hover:text-brand-950"
              role="menuitem"
            >
              <SiteIcon icon={siteNavIcon.docs} className={LUCIDE_LG} />
              <span>
                Open the product docs <span aria-hidden>→</span>
              </span>
            </Link>
            <p className="mt-1.5 pl-10 text-sm font-bold leading-6 text-ink-800/95 sm:pl-11">
              Setup, config, IAM, encryption, metrics, replication, and operations. Same docs as the
              product UI.
            </p>
          </div>
        </div>
      )}
    </div>
  );
}

export function Header(): JSX.Element {
  return (
    <header className="sticky top-0 z-10 border-b border-ink-200/60 bg-white/85 backdrop-blur dark:border-ink-700/60 dark:bg-ink-950/85">
      <div className="mx-auto flex max-w-6xl items-center justify-between px-6 py-4">
        <Link
          to="/"
          className="font-extrabold text-lg tracking-tight text-ink-900 dark:text-ink-50 transition-colors hover:text-brand-600 dark:hover:text-brand-300"
        >
          DeltaGlider <span className="text-brand-500">Proxy</span>
        </Link>
        <nav className="hidden items-center gap-5 text-sm font-semibold md:flex">
          <UseCasesMegaMenu />
          <Link
            to={DOCS_PATH}
            className="inline-flex items-center gap-2 rounded-lg bg-ink-950 px-3.5 py-2 pl-3 font-extrabold text-white shadow-lg shadow-ink-950/10 transition hover:bg-brand-700 dark:bg-brand-300 dark:text-ink-950 dark:hover:bg-brand-200"
          >
            <SiteIcon icon={siteNavIcon.docs} className="h-4 w-4 min-h-4 min-w-4" />
            Docs
          </Link>
          <ThemeToggle />
          <a
            href={REPO_URL}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-2 rounded-md border border-ink-300 bg-white py-1.5 pl-2.5 pr-3 text-ink-800 transition hover:border-brand-400 hover:text-brand-700 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100 dark:hover:border-brand-300 dark:hover:text-brand-300"
          >
            <SiteIcon icon={siteNavIcon.github} className="h-3.5 w-3.5" />
            GitHub
          </a>
        </nav>
        <details className="relative md:hidden">
          <summary className="list-none rounded-md border border-ink-300 bg-white px-3 py-1.5 text-sm font-bold text-ink-800 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100">
            Menu
          </summary>
          <div className="absolute right-0 z-20 mt-3 w-[min(100vw-1.5rem,20rem)] max-h-[80vh] overflow-y-auto rounded-xl border border-ink-200 bg-white p-2 shadow-xl dark:border-ink-700 dark:bg-ink-900">
            <div className="px-2 pb-1.5 pt-1 text-[10px] font-extrabold uppercase tracking-widest text-ink-400">
              Use cases
            </div>
            {USE_CASE_PATHS.map((item) => (
                <Link
                  key={item.to}
                  to={item.to}
                  className="flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm font-semibold text-ink-700 hover:bg-ink-100 dark:text-ink-200 dark:hover:bg-ink-800"
                >
                  <span
                    className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md bg-ink-100 text-ink-800 dark:bg-ink-800 dark:text-brand-200"
                    aria-hidden
                  >
                    <SiteIcon icon={item.icon} className={LUCIDE_SM} />
                  </span>
                  {item.navLabel}
                </Link>
            ))}
            <Link
              to={DOCS_PATH}
              className="mt-1 flex items-center gap-2.5 rounded-lg bg-ink-950 px-2.5 py-2.5 text-sm font-extrabold text-white transition hover:bg-brand-700 dark:bg-brand-300 dark:text-ink-950 dark:hover:bg-brand-200"
            >
              <SiteIcon icon={siteNavIcon.docs} className={LUCIDE_MD} />
              Docs
            </Link>
            <div className="border-t border-ink-200 px-2 py-2 dark:border-ink-700">
              <ThemeToggle />
            </div>
            <a
              href={REPO_URL}
              target="_blank"
              rel="noopener noreferrer"
              className="block rounded-lg px-2.5 py-2 text-sm font-semibold text-brand-700 transition hover:bg-brand-50 dark:text-brand-300 dark:hover:bg-ink-800"
            >
              <span className="inline-flex items-center gap-2">
                <SiteIcon icon={siteNavIcon.github} className="h-3.5 w-3.5" />
                GitHub
              </span>
            </a>
          </div>
        </details>
      </div>
    </header>
  );
}
