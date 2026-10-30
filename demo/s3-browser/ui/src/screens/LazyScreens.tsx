import { lazy, Suspense, type ComponentProps } from 'react';
import { Spin } from 'antd';

// Heavy admin/docs pages are lazy-loaded so the file-browser
// shell doesn't pay for Monaco / mermaid / recharts on first paint.
const AdminPage = lazy(() => import('../components/AdminPage'));
const DocsPage = lazy(() => import('../components/DocsPage'));

// Shared Suspense fallback for the lazy page chunks. Module-scope so it is
// not reallocated on every render.
const LAZY_FALLBACK = (
  <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', padding: 48 }}>
    <Spin size="large" />
  </div>
);

export function AdminScreen(props: ComponentProps<typeof AdminPage>) {
  return (
    <Suspense fallback={LAZY_FALLBACK}>
      <AdminPage {...props} />
    </Suspense>
  );
}

export function DocsScreen(props: ComponentProps<typeof DocsPage>) {
  return (
    <Suspense fallback={LAZY_FALLBACK}>
      <DocsPage {...props} />
    </Suspense>
  );
}
