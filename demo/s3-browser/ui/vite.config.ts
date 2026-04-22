import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  base: '/_/',
  define: {
    __BUILD_TIME__: JSON.stringify(new Date().toISOString()),
  },
  build: {
    sourcemap: true,
  },
  server: {
    proxy: {
      '/_/api': 'http://localhost:9000',
      '/_/health': 'http://localhost:9000',
      '/_/stats': 'http://localhost:9000',
      '/_/metrics': 'http://localhost:9000',
    },
  },
})
