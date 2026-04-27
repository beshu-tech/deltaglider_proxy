import type { RouteRecord } from 'vite-react-ssg';
import { Navigate } from 'react-router-dom';
import { Layout } from './components/Layout';
import { DOCS } from './docs-imports';
import { About } from './pages/About';
import { Docs } from './pages/Docs';
import { Landing } from './pages/Landing';
import { MinioMigration } from './pages/MinioMigration';
import { MultiCloud } from './pages/MultiCloud';
import { Privacy } from './pages/Privacy';
import { Regulated } from './pages/Regulated';
import { S3Migration } from './pages/S3Migration';
import { Terms } from './pages/Terms';
import { Versioning } from './pages/Versioning';

function RedirectToS3Migration(): JSX.Element {
  return <Navigate to="/s3-to-hetzner-wasabi/" replace />;
}

export const routes: RouteRecord[] = [
  {
    path: '/',
    element: <Layout />,
    entry: 'src/components/Layout.tsx',
    children: [
      { index: true, Component: Landing, entry: 'src/pages/Landing.tsx' },
      {
        path: 'regulated',
        Component: Regulated,
        entry: 'src/pages/Regulated.tsx',
      },
      {
        path: 'artifact-storage',
        Component: Versioning,
        entry: 'src/pages/Versioning.tsx',
      },
      {
        path: 'minio-migration',
        Component: MinioMigration,
        entry: 'src/pages/MinioMigration.tsx',
      },
      {
        path: 's3-saas-control-plane',
        Component: RedirectToS3Migration,
      },
      {
        path: 's3-to-hetzner-wasabi',
        Component: S3Migration,
        entry: 'src/pages/S3Migration.tsx',
      },
      {
        path: 'multi-cloud-control-plane',
        Component: MultiCloud,
        entry: 'src/pages/MultiCloud.tsx',
      },
      {
        path: 'docs',
        Component: Docs,
        entry: 'src/pages/Docs.tsx',
      },
      ...DOCS.map((doc): RouteRecord => ({
        path: `docs/${doc.id}`,
        element: <Docs initialDocId={doc.id} />,
        entry: 'src/pages/Docs.tsx',
      })),
      {
        path: 'about',
        Component: About,
        entry: 'src/pages/About.tsx',
      },
      {
        path: 'privacy',
        Component: Privacy,
        entry: 'src/pages/Privacy.tsx',
      },
      {
        path: 'terms',
        Component: Terms,
        entry: 'src/pages/Terms.tsx',
      },
    ],
  },
];
