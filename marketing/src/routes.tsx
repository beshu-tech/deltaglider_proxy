import type { RouteRecord } from 'vite-react-ssg';
import { Layout } from './components/Layout';
import { About } from './pages/About';
import { Landing } from './pages/Landing';
import { MinioMigration } from './pages/MinioMigration';
import { Privacy } from './pages/Privacy';
import { Regulated } from './pages/Regulated';
import { S3Saas } from './pages/S3Saas';
import { Terms } from './pages/Terms';
import { Versioning } from './pages/Versioning';

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
        Component: S3Saas,
        entry: 'src/pages/S3Saas.tsx',
      },
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
