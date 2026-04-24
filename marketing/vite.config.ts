import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import 'vite-react-ssg';

const BASE = process.env['MARKETING_BASE'] ?? '/deltaglider_proxy/';

export default defineConfig({
  base: BASE,
  plugins: [react(), tailwindcss()],
  ssgOptions: {
    script: 'async',
    formatting: 'none',
    dirStyle: 'nested',
    crittersOptions: false,
  },
});
