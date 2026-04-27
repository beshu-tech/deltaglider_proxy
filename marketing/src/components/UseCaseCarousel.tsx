import { useRef } from 'react';
import { Link } from 'react-router-dom';

const USE_CASE_GALLERY = [
  {
    to: '/s3-to-hetzner-wasabi/',
    kicker: 'AWS bill escape',
    title: 'Amazon S3 to Hetzner / Wasabi',
    body: 'Model cheaper backend storage plus compression while keeping IAM, policy, quotas, audit, metrics, and replication in DeltaGlider.',
    stat: 'Up to 95%+ cheaper',
    visual: ['AWS', 'DGP', 'Hetzner / Wasabi'],
    gradient: 'from-cyan-400 via-brand-400 to-emerald-300',
    bg: 'bg-cyan-950',
  },
  {
    to: '/artifact-storage/',
    kicker: 'Versioned binaries',
    title: 'Artifact and build retention',
    body: 'Store repeated release archives, installers, package catalogs, dumps, and model variants as compact deltas.',
    stat: '50-99% compression',
    visual: ['v1.zip', 'xdelta3 diff', 'v2.zip'],
    gradient: 'from-fuchsia-400 via-violet-400 to-cyan-300',
    bg: 'bg-violet-950',
  },
  {
    to: '/regulated/',
    kicker: 'Key custody',
    title: 'On-prem keys, encrypted cloud storage',
    body: 'The key never leaves your perimeter. The untrusted S3 SaaS only sees encrypted objects',
    stat: 'Key never leaves premises',
    visual: ['On-prem: DGP + key', 'encryption at rest', 'Untrusted Cloud Storage'],
    gradient: 'from-amber-300 via-orange-400 to-rose-400',
    bg: 'bg-amber-950',
  },
  {
    to: '/multi-cloud-control-plane/',
    kicker: 'Multi-cloud S3',
    title: 'One security layer, many backends',
    body: 'Expose one S3-compatible entry point while routing buckets to on-prem, Hetzner, Wasabi, or another backend with shared policy.',
    stat: 'Aliases + replication',
    visual: ['On-prem', 'DGP policy', 'Cloud archive'],
    gradient: 'from-sky-300 via-cyan-300 to-brand-300',
    bg: 'bg-sky-950',
  },
  {
    to: '/minio-migration/',
    kicker: 'Control-plane gap',
    title: 'Garage + DeltaGlider',
    body: 'Use Garage as the open-source storage layer, put DeltaGlider in front for IAM, OAuth, bucket policy, quotas, replication, and operator UI.',
    stat: 'OSS storage + Control Plane',
    visual: ['Garage (Storage)', '+', 'DeltaGlider (Control Plane)'],
    gradient: 'from-lime-300 via-brand-300 to-sky-400',
    bg: 'bg-emerald-950',
  },
] as const;

export function UseCaseCarousel(): JSX.Element {
  const scrollerRef = useRef<HTMLDivElement>(null);
  const scroll = (direction: 'left' | 'right') => {
    const node = scrollerRef.current;
    if (!node) return;
    node.scrollBy({
      left: direction === 'right' ? node.clientWidth * 0.82 : -node.clientWidth * 0.82,
      behavior: 'smooth',
    });
  };

  return (
    <section className="relative overflow-hidden border-y border-ink-200 bg-ink-950 py-10 text-white dark:border-ink-800">
      <div className="absolute inset-0 bg-[radial-gradient(circle_at_15%_20%,rgba(45,212,191,0.22),transparent_24rem),radial-gradient(circle_at_85%_10%,rgba(251,191,36,0.14),transparent_22rem)]" />
      <div className="relative mx-auto max-w-6xl px-6">
        <div className="flex flex-col gap-5 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <div className="text-xs font-extrabold uppercase tracking-[0.24em] text-brand-200">
              Use-case gallery
            </div>
            <h2 className="mt-2 max-w-3xl text-3xl font-black tracking-tight sm:text-4xl">
              Four ways teams use DeltaGlider as the object-storage control plane.
            </h2>
          </div>
          <div className="flex gap-2">
            <button
              type="button"
              className="rounded-full border border-white/15 bg-white/10 px-4 py-2 text-sm font-black text-white/80 transition hover:border-brand-200 hover:text-brand-100"
              onClick={() => scroll('left')}
              aria-label="Previous use case"
            >
              ←
            </button>
            <button
              type="button"
              className="rounded-full border border-white/15 bg-white/10 px-4 py-2 text-sm font-black text-white/80 transition hover:border-brand-200 hover:text-brand-100"
              onClick={() => scroll('right')}
              aria-label="Next use case"
            >
              →
            </button>
          </div>
        </div>

        <div
          ref={scrollerRef}
          className="mt-7 flex snap-x gap-5 overflow-x-auto pb-5 [scrollbar-width:thin]"
        >
          {USE_CASE_GALLERY.map((item, index) => (
            <Link
              key={item.to}
              to={item.to}
              className={`group relative min-w-[82%] snap-start overflow-hidden rounded-[2rem] border border-white/15 ${item.bg} p-6 shadow-2xl shadow-black/30 transition hover:-translate-y-1 hover:border-white/30 sm:min-w-[520px]`}
            >
              <div
                className={`absolute -right-20 -top-20 h-56 w-56 rounded-full bg-gradient-to-br ${item.gradient} opacity-30 blur-2xl transition group-hover:scale-125`}
              />
              <div className="relative">
                <div className="flex items-center justify-between gap-4">
                  <div className="rounded-full bg-white/10 px-3 py-1 text-[11px] font-extrabold uppercase tracking-[0.18em] text-white/75">
                    {item.kicker}
                  </div>
                  <div className="font-mono text-sm font-black text-white/35">
                    0{index + 1}
                  </div>
                </div>

                <div className="mt-8 grid gap-5 sm:grid-cols-[1fr_0.9fr] sm:items-end">
                  <div>
                    <h3 className="text-3xl font-black leading-none tracking-tight">
                      {item.title}
                    </h3>
                    <p className="mt-4 text-sm leading-6 text-white/72">{item.body}</p>
                    <div className="mt-6 inline-flex items-center gap-2 rounded-full bg-white px-4 py-2 text-sm font-black text-ink-950">
                      {item.stat} <span aria-hidden>→</span>
                    </div>
                  </div>

                  <div className="rounded-3xl border border-white/15 bg-black/20 p-4 backdrop-blur">
                    <div className="grid gap-3">
                      {item.visual.map((label, visualIndex) => (
                        <div
                          key={label}
                          className={`rounded-2xl border border-white/10 px-4 py-3 text-center text-sm font-black ${
                            visualIndex === 1
                              ? `bg-gradient-to-r ${item.gradient} text-ink-950`
                              : 'bg-white/10 text-white'
                          }`}
                        >
                          {label}
                        </div>
                      ))}
                    </div>
                  </div>
                </div>
              </div>
            </Link>
          ))}
        </div>
      </div>
    </section>
  );
}
