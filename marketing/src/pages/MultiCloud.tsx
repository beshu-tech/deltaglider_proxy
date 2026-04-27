import { FeatureCard } from '../components/FeatureCard';
import { MailtoCTA } from '../components/MailtoCTA';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { multiCloudMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const SUBJECT = 'Multi-cloud S3 control plane';

const BACKENDS = [
  {
    name: 'On-prem',
    role: 'Hot tier',
    detail: 'Latest 90 days, fast local reads, local key custody.',
  },
  {
    name: 'Hetzner',
    role: 'Archive tier',
    detail: 'Older encrypted objects, lower storage cost, S3-compatible backend.',
  },
  {
    name: 'Wasabi / other S3',
    role: 'Second target',
    detail: 'Optional replication target for provider optionality or DR.',
  },
] as const;

const CONTROL_ROWS = [
  ['Bucket aliasing', 'Expose one stable bucket name while routing to different real backend buckets.'],
  ['Unified Access Control', 'Users, groups, S3 keys, OAuth/OIDC mapping, and ABAC stay in one control plane.'],
  ['Cross-cloud replication', 'Copy objects between buckets or backends with schedule, run history, failures, and optional delete replication.'],
  ['Encryption at rest', 'Encrypt before leaving your premises, then store ciphertext in a cheaper cloud backend.'],
  ['Lifecycle-style retention', 'Keep recent files locally and replicate older data in cheaper cloud storage.'],
  ['Operational evidence', 'Prometheus metrics, audit entries, replication state, and admin UI visibility stay consistent.'],
] as const;

export function MultiCloud(): JSX.Element {
  return (
    <>
      <SEO meta={multiCloudMeta} />

      <section className="relative isolate overflow-hidden bg-ink-950 text-white">
        <div className="absolute inset-0 -z-10 bg-[radial-gradient(circle_at_12%_18%,rgba(45,212,191,.25),transparent_26rem),radial-gradient(circle_at_82%_22%,rgba(96,165,250,.22),transparent_26rem),linear-gradient(135deg,#020617,#111827_48%,#042f2e)]" />
        <div className="absolute inset-0 -z-10 opacity-30 [background-image:linear-gradient(120deg,rgba(255,255,255,.08)_1px,transparent_1px)] [background-size:36px_36px]" />

        <div className="mx-auto grid max-w-6xl gap-10 px-6 py-16 lg:grid-cols-[0.95fr_1.05fr] lg:items-center lg:py-24">
          <div>
            <div className="inline-flex rounded-full border border-brand-300/40 bg-brand-300/10 px-4 py-2 text-xs font-extrabold uppercase tracking-[0.22em] text-brand-200">
              Multi-cloud control plane
            </div>
            <h1 className="mt-6 max-w-4xl text-5xl font-black tracking-tight sm:text-6xl lg:text-7xl">
              One S3 security layer.
              <span className="block bg-gradient-to-r from-brand-200 via-cyan-200 to-sky-200 bg-clip-text text-transparent">
                Many storage backends.
              </span>
            </h1>
            <p className="mt-6 max-w-2xl text-lg leading-relaxed text-ink-200 sm:text-xl">
              Use DeltaGlider as the stable S3-compatible entry point across
              on-prem storage, Hetzner, Wasabi, or another backend. Keep access
              control, bucket aliases, encryption, replication, audit, and
              operator workflows in one place.
            </p>
            <div className="mt-8 flex flex-wrap gap-3">
              <MailtoCTA subject={SUBJECT} label="Map our backends" />
              <a
                href="#example"
                className="inline-flex items-center gap-2 rounded-lg border border-white/20 bg-white/10 px-5 py-3 font-semibold text-white backdrop-blur transition hover:border-brand-200 hover:text-brand-100"
              >
                See retention example <span aria-hidden>↓</span>
              </a>
            </div>
          </div>

          <div className="rounded-[2rem] border border-white/15 bg-white/10 p-4 shadow-2xl shadow-black/30 backdrop-blur-xl">
            <div className="rounded-[1.5rem] bg-ink-950/80 p-5 ring-1 ring-white/10">
              <div className="grid gap-4">
                <div className="rounded-2xl border border-brand-300/40 bg-brand-300/10 p-5">
                  <div className="text-xs font-extrabold uppercase tracking-[0.22em] text-brand-200">
                    Stable S3 endpoint
                  </div>
                  <div className="mt-2 text-3xl font-black">DeltaGlider</div>
                  <p className="mt-2 text-sm leading-6 text-ink-300">
                    IAM, OAuth, ABAC, aliases, encryption, quotas, replication,
                    metrics, audit.
                  </p>
                </div>
                <div className="grid gap-3 sm:grid-cols-3">
                  {BACKENDS.map((backend) => (
                    <div
                      key={backend.name}
                      className="rounded-2xl border border-white/10 bg-white/5 p-4"
                    >
                      <div className="text-lg font-black">{backend.name}</div>
                      <div className="mt-1 text-xs font-extrabold uppercase tracking-widest text-cyan-200">
                        {backend.role}
                      </div>
                      <p className="mt-3 text-xs leading-5 text-ink-300">
                        {backend.detail}
                      </p>
                    </div>
                  ))}
                </div>
              </div>
            </div>
          </div>
        </div>
      </section>

      <Section
        eyebrow="Control layer"
        title="One policy surface over many object stores."
        intro="Backends move bytes. DeltaGlider keeps the app-facing security and operations contract stable."
      >
        <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
          {CONTROL_ROWS.map(([title, body]) => (
            <FeatureCard key={title} title={title} body={body} />
          ))}
        </div>
      </Section>

      <Section
        eyebrow="Retention pattern"
        title="Keep hot data on-prem. Encrypt and replicate older data out."
        intro="A practical lifecycle-style pattern: preserve fast local access for recent data, then move older date-partitioned objects to cheaper cloud storage as ciphertext."
      >
        <div
          id="example"
          className="overflow-hidden rounded-[2rem] border border-ink-200 bg-white shadow-2xl shadow-brand-950/10 dark:border-ink-700 dark:bg-ink-950"
        >
          <div className="grid gap-0 lg:grid-cols-3">
            <div className="bg-ink-950 p-6 text-white">
              <div className="text-xs font-extrabold uppercase tracking-[0.2em] text-brand-200">
                0-3 months
              </div>
              <h2 className="mt-4 text-3xl font-black">On-prem hot tier</h2>
              <p className="mt-3 text-sm leading-6 text-ink-300">
                Keep the newest objects close to applications. Reads are local;
                keys and policy stay under your control.
              </p>
            </div>
            <div className="bg-gradient-to-br from-brand-300 to-cyan-300 p-6 text-ink-950">
              <div className="text-xs font-extrabold uppercase tracking-[0.2em]">
                DeltaGlider rule
              </div>
              <h2 className="mt-4 text-3xl font-black">Encrypt + replicate</h2>
              <p className="mt-3 text-sm font-semibold leading-6">
                Schedule replication for older prefixes. DGP writes encrypted
                objects to the target backend and records run history/failures.
              </p>
            </div>
            <div className="bg-sky-950 p-6 text-white">
              <div className="text-xs font-extrabold uppercase tracking-[0.2em] text-sky-200">
                3+ months
              </div>
              <h2 className="mt-4 text-3xl font-black">Hetzner archive</h2>
              <p className="mt-3 text-sm leading-6 text-sky-100">
                Store lower-cost ciphertext in cloud object storage. Apps still
                talk to the same DGP-controlled S3-compatible entry point.
              </p>
            </div>
          </div>
        </div>
        <p className="mt-5 text-sm leading-6 text-ink-600 dark:text-ink-300">
          This is a lifecycle-style placement pattern, not a claim of complete
          Amazon S3 Lifecycle parity. If you require legal hold, Object Lock, or
          provider-native lifecycle transitions, keep those backend controls in
          the architecture.
        </p>
      </Section>

      <Section
        eyebrow="Implementation hooks"
        title="The primitives are already in the product."
      >
        <div className="grid gap-5 md:grid-cols-2">
          <FeatureCard
            title="Backend routing and aliases"
            body="Route buckets to named backends and map virtual bucket names to real backend bucket names."
            sourceLabel="docs/product/reference/configuration.md"
            sourceHref={`${REPO_URL}/blob/main/docs/product/reference/configuration.md`}
          />
          <FeatureCard
            title="Object replication state"
            body="Replication tracks scheduler state, run history, continuation tokens, failures, and paused rules."
            sourceLabel="src/replication"
            sourceHref={`${REPO_URL}/tree/main/src/replication`}
          />
          <FeatureCard
            title="Proxy-side encryption"
            body="Encrypt before data leaves the trusted runtime; the cloud backend stores ciphertext."
            sourceLabel="docs/product/reference/encryption-at-rest.md"
            sourceHref={`${REPO_URL}/blob/main/docs/product/reference/encryption-at-rest.md`}
          />
          <FeatureCard
            title="Unified access control"
            body="IAM users, groups, access keys, OAuth/OIDC, ABAC, public prefixes, and audit stay in DeltaGlider."
            sourceLabel="src/iam"
            sourceHref={`${REPO_URL}/tree/main/src/iam`}
          />
        </div>
      </Section>

      <Section
        eyebrow="Next step"
        title="Draw your backend map."
        intro="Bring the buckets, retention windows, cloud targets, and controls you need to preserve. We will map aliases, replication, encryption, and operational ownership."
      >
        <MailtoCTA subject={SUBJECT} label="Design the control plane" />
      </Section>
    </>
  );
}
