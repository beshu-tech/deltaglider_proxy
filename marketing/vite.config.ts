import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import 'vite-react-ssg';

const BASE = process.env['MARKETING_BASE'] ?? '/';

export default defineConfig({
  base: BASE,
  plugins: [react(), tailwindcss()],
  build: {
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (
            id.includes('react-markdown') ||
            id.includes('remark-') ||
            id.includes('rehype-') ||
            id.includes('micromark') ||
            id.includes('unified') ||
            id.includes('mdast') ||
            id.includes('hast') ||
            id.includes('unist')
          ) {
            return 'markdown';
          }
        },
      },
    },
  },
  ssgOptions: {
    script: 'async',
    formatting: 'none',
    dirStyle: 'nested',
    crittersOptions: false,
  },
});
