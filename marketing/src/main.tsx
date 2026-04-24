import { ViteReactSSG } from 'vite-react-ssg';
import { routes } from './routes';
import './styles.css';

export const createRoot = ViteReactSSG({ routes });
