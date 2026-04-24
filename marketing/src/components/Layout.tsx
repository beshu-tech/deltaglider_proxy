import { Outlet } from 'react-router-dom';
import { Footer } from './Footer';
import { Header } from './Header';

export function Layout(): JSX.Element {
  return (
    <div className="min-h-screen flex flex-col bg-ink-50 dark:bg-ink-900 text-ink-900 dark:text-ink-100">
      <Header />
      <main className="flex-1">
        <Outlet />
      </main>
      <Footer />
    </div>
  );
}
