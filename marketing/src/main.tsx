import { ViteReactSSG } from 'vite-react-ssg';
import { routes } from './routes';
import './styles.css';

if (typeof window !== 'undefined') {
  // This site has no React Router loaders. Pre-seeding these avoids runtime
  // static-loader JSON fetches that can fall back to HTML on static hosts.
  const ssgWindow = window as typeof window & {
    __VITE_REACT_SSG_STATIC_LOADER_MANIFEST__?: Record<string, string>;
    __VITE_REACT_SSG_STATIC_LOADER_DATA__?: Record<string, unknown>;
  };
  ssgWindow.__VITE_REACT_SSG_STATIC_LOADER_MANIFEST__ ??= {};
  ssgWindow.__VITE_REACT_SSG_STATIC_LOADER_DATA__ ??= {};
}

export const createRoot = ViteReactSSG({
  routes,
  basename: import.meta.env.BASE_URL,
});
