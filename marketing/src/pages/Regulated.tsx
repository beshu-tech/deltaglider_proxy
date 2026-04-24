import { FeatureCard } from '../components/FeatureCard';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
import { RoadmapRibbon } from '../components/RoadmapRibbon';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { regulatedMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const SUBJECT = 'Regulated workloads inquiry';

export function Regulated(): JSX.Element {
  return (
    <>
      <SEO meta={regulatedMeta} />
      <Hero
        eyebrow="Use case · regulated workloads"
        headline="Your data never leaves your key. Your key never leaves your premises."
        subhead="Compliance teams reject public-cloud S3 because customer-managed keys end up inside AWS KMS. DeltaGlider Proxy encrypts every object with a key that lives in your environment — and only your environment."
        cta={
          <>
            <MailtoCTA
              subject={SUBJECT}
              label="Talk to compliance-aware engineers"
            />
          </>
        }
        illustration={
          <img
            src="screenshots/advanced_security.jpg"
            alt="DeltaGlider Proxy advanced security settings"
            loading="eager"
            className="block w-full h-auto"
          />
        }
      />
      <Section
        eyebrow="The problem"
        title="On-prem is expensive. Public S3 is non-negotiable for legal."
        intro="So you're stuck running storage you'd rather not own, paying egress for replication, and explaining to auditors why your KMS provider sees every encryption decision."
      >
        <></>
      </Section>
      <Section
        eyebrow="What ships today"
        title="Three things you need before legal will sign."
      >
        <div className="grid gap-5 md:grid-cols-3">
          <FeatureCard
            title="AES-256-GCM, key from your environment"
            body={
              <>
                The encryption key is supplied at process start via{' '}
                <code className="text-sm">DGP_ENCRYPTION_KEY</code>. It is held
                in memory only — never written to the storage backend, never
                sent to AWS KMS, zeroized on shutdown. Per-object 12-byte random
                IV plus 16-byte auth tag.
              </>
            }
            sourceLabel="src/storage/encrypting.rs"
            sourceHref={`${REPO_URL}/blob/main/src/storage/encrypting.rs`}
          />
          <FeatureCard
            title="Encrypted IAM database"
            body={
              <>
                Users, groups, ABAC policies, and OAuth providers live in an
                encrypted SQLCipher database. The passphrase is yours. The
                database can be synced across multiple proxy instances via S3 as
                an encrypted blob, with ETag-based polling for hot reload.
              </>
            }
            sourceLabel="src/config_db_sync.rs"
            sourceHref={`${REPO_URL}/blob/main/src/config_db_sync.rs`}
          />
          <FeatureCard
            title="ABAC, AWS-grammar policies"
            body={
              <>
                Per-user S3 credentials, groups, prefix-scoped resources, and
                conditions on IP ranges and prefix patterns. Parsed by{' '}
                <code className="text-sm">iam-rs</code>: same policy grammar as
                AWS IAM, so your existing review process still applies.
              </>
            }
            sourceLabel="src/iam/permissions.rs"
            sourceHref={`${REPO_URL}/blob/main/src/iam/permissions.rs`}
          />
        </div>
      </Section>
      <Section
        eyebrow="On the way"
        title="What's next on the regulated roadmap."
      >
        <RoadmapRibbon
          title="Cross-backend replication"
          body="Eventually-consistent replication to a secondary backend (primary fast/local, secondary durable/cloud). Per-bucket configuration, in-process queue, crash-safe via SQLite-persisted state. Designed; not yet shipped."
          href={`${REPO_URL}/blob/main/future/REPLICATION.md`}
          hrefLabel="future/REPLICATION.md"
        />
      </Section>
      <Section
        eyebrow="Next step"
        title="Tell us about your compliance perimeter."
        intro="If you have data classification rules, residency constraints, or KMS posture we should design around, send a short note. We answer."
      >
        <MailtoCTA subject={SUBJECT} label="Email us" />
      </Section>
    </>
  );
}
