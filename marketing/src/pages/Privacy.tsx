import { MailtoCTA } from '../components/MailtoCTA';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { privacyMeta } from '../seo/pages';
import { CONTACT_EMAIL, ORG_NAME } from '../seo/schema';

const ITEMS = [
  {
    title: 'Website data',
    body: 'This marketing site is static. We do not run application accounts, payment flows, or product telemetry from this site.',
  },
  {
    title: 'Contact data',
    body: `If you email ${CONTACT_EMAIL}, we use your message and contact details to respond to your inquiry and manage the business relationship.`,
  },
  {
    title: 'Repository and third-party links',
    body: 'Links to GitHub, documentation, or external services are governed by those services when you visit them.',
  },
  {
    title: 'Product deployments',
    body: 'DeltaGlider Proxy is software you run in your own environment. Your object data, IAM database, logs, and configuration stay under your operational control unless you choose to share them.',
  },
  {
    title: 'Retention',
    body: 'Business correspondence is retained as long as needed to answer requests, support evaluations, meet legal obligations, and maintain normal company records.',
  },
  {
    title: 'Your rights',
    body: `For privacy questions, access requests, or deletion requests, contact ${CONTACT_EMAIL}.`,
  },
];

export function Privacy(): JSX.Element {
  return (
    <>
      <SEO meta={privacyMeta} />
      <Section
        eyebrow="Privacy"
        title="Privacy Policy"
        intro={`${ORG_NAME} keeps the marketing site simple and the product deployment model customer-controlled. This page explains the limited data we handle through the website and business contact channels.`}
      >
        <div className="grid gap-5 md:grid-cols-2">
          {ITEMS.map((item) => (
            <div
              key={item.title}
              className="rounded-xl border border-ink-200 bg-white p-6 dark:border-ink-700 dark:bg-ink-800/40"
            >
              <h3 className="text-lg font-extrabold text-ink-900 dark:text-ink-50">
                {item.title}
              </h3>
              <p className="mt-2 text-[15px] leading-relaxed text-ink-600 dark:text-ink-300">
                {item.body}
              </p>
            </div>
          ))}
        </div>
      </Section>
      <Section
        eyebrow="Contact"
        title="Questions about privacy?"
        intro="Write to us and include enough context for us to identify the relevant communication or request."
      >
        <MailtoCTA subject="DeltaGlider Proxy privacy question" label="Email privacy contact" />
      </Section>
    </>
  );
}
