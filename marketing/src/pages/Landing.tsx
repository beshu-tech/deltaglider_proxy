import { Link } from 'react-router-dom';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
import { ProofStrip } from '../components/ProofStrip';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { landingMeta } from '../seo/pages';

interface NicheCard {
  to: string;
  voice: string;
  who: string;
  payoff: string;
}

const NICHES: readonly NicheCard[] = [
  {
    to: '/regulated/',
    voice: '"We store confidential data and can\'t touch raw S3."',
    who: 'Regulated workloads',
    payoff:
      'Encryption at rest with a key that lives in your environment, not in AWS KMS. ABAC IAM with AWS-grammar policies.',
  },
  {
    to: '/versioning/',
    voice: '"Our S3 bill is mostly almost-identical binaries."',
    who: 'Artifact versioning',
    payoff:
      'Transparent xdelta3 compression on JARs, tarballs, ISOs, SQL dumps. Up to 95% storage savings, no client changes.',
  },
  {
    to: '/minio-migration/',
    voice: '"We were happy on MinIO until the license changed."',
    who: 'MinIO migration',
    payoff:
      'Drop-in S3 with per-user credentials, groups, OAuth/OIDC group mapping. Single binary, encrypted config DB.',
  },
];

export function Landing(): JSX.Element {
  return (
    <>
      <SEO meta={landingMeta} />
      <Hero
        eyebrow="S3-compatible · open source"
        headline="Cloud storage up to 10× cheaper — without telling your apps."
        subhead="DeltaGlider Proxy speaks the S3 API on the wire and silently deduplicates your versioned binaries with delta compression. Drop-in SigV4, ABAC IAM, AES-256-GCM encryption at rest."
        cta={
          <>
            <MailtoCTA
              subject="DeltaGlider Proxy inquiry"
              label="Talk to us"
            />
            <a
              href="https://github.com/beshu-tech/deltaglider_proxy"
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-2 rounded-lg border border-ink-300 bg-white px-5 py-3 font-semibold text-ink-800 hover:border-brand-400 hover:text-brand-700 dark:border-ink-600 dark:bg-ink-800 dark:text-ink-100 dark:hover:border-brand-300 dark:hover:text-brand-300"
            >
              View on GitHub
            </a>
          </>
        }
        illustration={
          <img
            src="screenshots/filebrowser.jpg"
            alt="DeltaGlider Proxy file browser showing per-bucket compression statistics"
            loading="eager"
            className="block w-full h-auto"
          />
        }
      />
      <ProofStrip />
      <Section
        eyebrow="Pick your door"
        title="Three different problems. Three different pages."
        intro="DeltaGlider Proxy isn't only about cheap storage. It's also about encryption you control and ABAC IAM that survives MinIO migrations. Pick the page that matches your problem — the pitch is sharper there."
      >
        <div className="grid gap-5 md:grid-cols-3">
          {NICHES.map((niche) => (
            <Link
              key={niche.to}
              to={niche.to}
              className="group rounded-xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/40 hover:border-brand-400 hover:shadow-md transition-all"
            >
              <div className="text-sm italic text-ink-500 dark:text-ink-400">
                {niche.voice}
              </div>
              <div className="mt-4 text-xl font-extrabold text-ink-900 dark:text-ink-50">
                {niche.who}
              </div>
              <p className="mt-2 text-[15px] text-ink-600 dark:text-ink-300 leading-relaxed">
                {niche.payoff}
              </p>
              <div className="mt-5 inline-flex items-center gap-1 text-sm font-semibold text-brand-700 group-hover:text-brand-800 dark:text-brand-300 dark:group-hover:text-brand-200">
                Read the pitch <span aria-hidden>→</span>
              </div>
            </Link>
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Closing question"
        title="Ready to put numbers on your own data?"
        intro="If any of the three pitches above sounded like your team, get in touch and we'll help you scope it."
      >
        <div className="flex flex-wrap gap-3">
          <MailtoCTA
            subject="DeltaGlider Proxy inquiry"
            label="Email us"
          />
        </div>
      </Section>
    </>
  );
}
