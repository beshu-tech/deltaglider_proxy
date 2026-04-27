import { ChecklistGrid } from '../components/ChecklistGrid';
import { FeatureCard } from '../components/FeatureCard';
import { Hero } from '../components/Hero';
import { MailtoCTA } from '../components/MailtoCTA';
import { ScreenshotFrame } from '../components/ScreenshotFrame';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { regulatedMeta } from '../seo/pages';
import { REPO_URL } from '../seo/schema';

const SUBJECT = 'Regulated workloads inquiry';

const CONTROLS = [
  'AES-256-GCM proxy-side encryption at rest',
  'Key supplied from your environment',
  'Encrypted SQLCipher IAM/config database',
  'ABAC permissions with source-IP and prefix conditions',
  'Public read-only prefixes for controlled publishing',
  'Soft bucket quotas and bucket freeze',
  'Object replication through the engine',
  'Audit ring and Prometheus metrics',
];

const SCENARIOS = [
  {
    title: 'Untrusted low-cost SaaS',
    body: 'A regulated team wants ultra-cheap S3-compatible storage, but cannot trust the provider with plaintext. DeltaGlider encrypts before the backend; the key stays on trusted premises.',
  },
  {
    title: 'Partner downloads',
    body: 'A release bucket needs public read-only access for a few prefixes, while everything else remains private behind IAM.',
  },
  {
    title: 'Secondary copy requirements',
    body: 'A regulated workload needs a cloud copy or offsite prefix. Replication runs through the proxy, so encryption and compression behavior stay consistent.',
  },
];

export function Regulated(): JSX.Element {
  return (
    <>
      <SEO meta={regulatedMeta} />
      <Hero
        eyebrow="Use case · regulated workloads"
        headline="Encryption at rest: Using cheap providers that you don't trust."
        subhead="DeltaGlider encrypts objects before they reach the backend. The cryptographic key stays in your trusted environment, while compression can further reduce the storage bill."
        cta={
          <>
            <MailtoCTA
              subject={SUBJECT}
              label="Talk to compliance-aware engineers"
            />
          </>
        }
        illustration={
          <ScreenshotFrame
            src="screenshots/advanced_security.jpg"
            alt="DeltaGlider Proxy advanced security settings"
            caption="Advanced security controls for regulated deployments: listener settings, TLS, cookie policy, rate limits, and other runtime guardrails."
            priority
          />
        }
      />
      <Section
        eyebrow="Controls"
        title="Controls built into the proxy."
        intro="The storage provider sees ciphertext. Your runtime keeps the key, policy, and operational controls."
      >
        <ChecklistGrid items={CONTROLS} />
      </Section>
      <Section
        eyebrow="Real-world scenarios"
        title="Common compliance patterns."
        intro="The proxy is useful when storage policy needs to stay close to your runtime, not hidden inside one vendor account."
      >
        <div className="grid gap-5 md:grid-cols-3">
          {SCENARIOS.map((scenario) => (
            <FeatureCard
              key={scenario.title}
              title={scenario.title}
              body={scenario.body}
            />
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Evidence"
        title="Security features with code behind them."
      >
        <div className="grid gap-5 md:grid-cols-3">
          <FeatureCard
            title="Proxy-side encryption"
            body={
              <>
                Objects are encrypted with AES-256-GCM before they reach the
                backend. The key is supplied via{' '}
                <code className="text-sm">DGP_ENCRYPTION_KEY</code> and never
                leaves your trusted runtime environment.
              </>
            }
            sourceLabel="src/storage/encrypting.rs"
            sourceHref={`${REPO_URL}/blob/main/src/storage/encrypting.rs`}
          />
          <FeatureCard
            title="Encrypted config database"
            body={
              <>
                Users, groups, policies, and OAuth providers live in SQLCipher.
                The encrypted DB can sync across proxy instances through S3.
              </>
            }
            sourceLabel="src/config_db_sync.rs"
            sourceHref={`${REPO_URL}/blob/main/src/config_db_sync.rs`}
          />
          <FeatureCard
            title="ABAC access control"
            body={
              <>
                Per-user S3 credentials, groups, prefix-scoped resources, and
                conditions on IP ranges and prefixes using AWS-style policy
                grammar.
              </>
            }
            sourceLabel="src/iam/permissions.rs"
            sourceHref={`${REPO_URL}/blob/main/src/iam/permissions.rs`}
          />
        </div>
      </Section>
      <Section
        eyebrow="Object replication"
        title="Copy data through the same control plane."
        intro="Replication rules copy objects between buckets or backends while preserving proxy-side encryption and compression behavior."
      >
        <ScreenshotFrame
          src="screenshots/object-replication.jpg"
          alt="DeltaGlider Proxy object replication configuration"
          caption="Pause rules, run now, review history and failures, and replicate deletes when needed."
        />
      </Section>
      <Section
        eyebrow="Next step"
        title="Map your storage controls."
        intro="Send the requirements that matter: keys, identity, quotas, replication, audit, and deployment constraints."
      >
        <MailtoCTA subject={SUBJECT} label="Email us" />
      </Section>
    </>
  );
}
