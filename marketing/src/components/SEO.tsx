import { Head } from 'vite-react-ssg';
import type { PageMeta } from '../seo/pages';
import { SITE_URL } from '../seo/schema';
import { JsonLd } from './JsonLd';

interface SEOProps {
  meta: PageMeta;
}

export function SEO({ meta }: SEOProps): JSX.Element {
  const canonical = `${SITE_URL}${meta.path}`;
  return (
    <>
      <Head>
        <title>{meta.title}</title>
        <meta name="description" content={meta.description} />
        <link rel="canonical" href={canonical} />
        <meta property="og:type" content="website" />
        <meta property="og:title" content={meta.title} />
        <meta property="og:description" content={meta.description} />
        <meta property="og:url" content={canonical} />
        <meta property="og:image" content={meta.ogImage} />
        <meta property="og:site_name" content="DeltaGlider Proxy" />
        <meta name="twitter:card" content="summary_large_image" />
        <meta name="twitter:title" content={meta.title} />
        <meta name="twitter:description" content={meta.description} />
        <meta name="twitter:image" content={meta.ogImage} />
      </Head>
      {meta.jsonLd.map((payload, idx) => (
        <JsonLd key={idx} payload={payload} />
      ))}
    </>
  );
}
