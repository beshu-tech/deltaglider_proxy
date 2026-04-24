import type { RouteRecord } from 'vite-react-ssg';
import { Layout } from './components/Layout';
import { Landing } from './pages/Landing';
import { MinioMigration } from './pages/MinioMigration';
import { Regulated } from './pages/Regulated';
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
        path: 'versioning',
        Component: Versioning,
        entry: 'src/pages/Versioning.tsx',
      },
      {
        path: 'minio-migration',
        Component: MinioMigration,
        entry: 'src/pages/MinioMigration.tsx',
      },
    ],
  },
];
