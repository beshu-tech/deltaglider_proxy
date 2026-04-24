import { Head } from 'vite-react-ssg';
import type { JsonLdPayload } from '../seo/pages';

interface JsonLdProps {
  payload: JsonLdPayload;
}

export function JsonLd({ payload }: JsonLdProps): JSX.Element {
  return (
    <Head>
      <script type="application/ld+json">{JSON.stringify(payload)}</script>
    </Head>
  );
}
