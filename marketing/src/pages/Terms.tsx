import { MailtoCTA } from '../components/MailtoCTA';
import { SEO } from '../components/SEO';
import { Section } from '../components/Section';
import { termsMeta } from '../seo/pages';
import { ORG_NAME, REPO_URL } from '../seo/schema';

const TERMS = [
  {
    title: 'Website use',
    body: 'You may browse this website for product information, documentation, and project links. Do not misuse the site, attempt to disrupt it, or use it for unlawful activity.',
  },
  {
    title: 'Software license',
    body: 'DeltaGlider Proxy is distributed under the license published in the GitHub repository. The repository license controls your rights to copy, modify, and run the software.',
  },
  {
    title: 'No hosted storage service',
    body: `${ORG_NAME} does not host your DeltaGlider object data through this marketing site. Deployments are operated by you unless a separate written agreement says otherwise.`,
  },
  {
    title: 'Evaluation and support',
    body: 'Any commercial evaluation, support, or professional service is subject to the written agreement, order form, or statement of work agreed between the parties.',
  },
  {
    title: 'Customer data',
    body: 'You remain responsible for the data, credentials, configuration, and policies you process through your own DeltaGlider deployment.',
  },
  {
    title: 'No warranty on website content',
    body: 'Website content is provided for product information. It may change as the software evolves and should not replace the repository, documentation, or a written agreement.',
  },
  {
    title: 'Limitation of liability',
    body: 'To the maximum extent permitted by law, Beshu Tech is not liable for indirect, incidental, special, consequential, or punitive damages arising from use of this website.',
  },
  {
    title: 'Changes',
    body: 'We may update these terms as the site, product, or commercial offering changes. Continued use of the site means you accept the updated terms.',
  },
];

export function Terms(): JSX.Element {
  return (
    <>
      <SEO meta={termsMeta} />
      <Section
        eyebrow="Terms"
        title="Terms of Service"
        intro="These terms govern use of the DeltaGlider Proxy marketing site. Software use is governed by the repository license unless a separate written agreement applies."
      >
        <div className="rounded-2xl border border-brand-200 bg-brand-50/70 p-6 text-sm leading-relaxed text-brand-900 dark:border-brand-800 dark:bg-brand-950/30 dark:text-brand-100">
          For source code and license details, see the{' '}
          <a
            href={REPO_URL}
            target="_blank"
            rel="noopener noreferrer"
            className="font-extrabold underline decoration-brand-400 underline-offset-4"
          >
            DeltaGlider Proxy repository
          </a>
          .
        </div>
        <div className="mt-8 grid gap-5 md:grid-cols-2">
          {TERMS.map((item) => (
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
        title="Need commercial terms?"
        intro="For support, evaluation, or deployment help, contact the team."
      >
        <MailtoCTA subject="DeltaGlider Proxy commercial terms" label="Contact Beshu Tech" />
      </Section>
    </>
  );
}
